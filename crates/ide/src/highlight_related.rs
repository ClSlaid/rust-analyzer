use std::iter;

use hir::{DescendPreference, Semantics};
use ide_db::{
    base_db::{FileId, FilePosition, FileRange},
    defs::{Definition, IdentClass},
    helpers::pick_best_token,
    search::{FileReference, ReferenceCategory, SearchScope},
    syntax_helpers::node_ext::{
        for_each_break_and_continue_expr, for_each_tail_expr, full_path_of_name_ref, walk_expr,
    },
    FxHashSet, RootDatabase,
};
use syntax::{
    ast::{self, HasLoopBody},
    match_ast, AstNode,
    SyntaxKind::{self, IDENT, INT_NUMBER},
    SyntaxToken, TextRange, T,
};

use crate::{navigation_target::ToNav, NavigationTarget, TryToNav};

#[derive(PartialEq, Eq, Hash)]
pub struct HighlightedRange {
    pub range: TextRange,
    // FIXME: This needs to be more precise. Reference category makes sense only
    // for references, but we also have defs. And things like exit points are
    // neither.
    pub category: Option<ReferenceCategory>,
}

#[derive(Default, Clone)]
pub struct HighlightRelatedConfig {
    pub references: bool,
    pub exit_points: bool,
    pub break_points: bool,
    pub closure_captures: bool,
    pub yield_points: bool,
}

// Feature: Highlight Related
//
// Highlights constructs related to the thing under the cursor:
//
// . if on an identifier, highlights all references to that identifier in the current file
// .. additionally, if the identifier is a trait in a where clause, type parameter trait bound or use item, highlights all references to that trait's assoc items in the corresponding scope
// . if on an `async` or `await` token, highlights all yield points for that async context
// . if on a `return` or `fn` keyword, `?` character or `->` return type arrow, highlights all exit points for that context
// . if on a `break`, `loop`, `while` or `for` token, highlights all break points for that loop or block context
// . if on a `move` or `|` token that belongs to a closure, highlights all captures of the closure.
//
// Note: `?`, `|` and `->` do not currently trigger this behavior in the VSCode editor.
pub(crate) fn highlight_related(
    sema: &Semantics<'_, RootDatabase>,
    config: HighlightRelatedConfig,
    pos @ FilePosition { offset, file_id }: FilePosition,
) -> Option<Vec<HighlightedRange>> {
    let _p = profile::span("highlight_related");
    let syntax = sema.parse(file_id).syntax().clone();

    let token = pick_best_token(syntax.token_at_offset(offset), |kind| match kind {
        T![?] => 4, // prefer `?` when the cursor is sandwiched like in `await$0?`
        T![->] => 4,
        kind if kind.is_keyword() => 3,
        IDENT | INT_NUMBER => 2,
        T![|] => 1,
        _ => 0,
    })?;
    // most if not all of these should be re-implemented with information seeded from hir
    match token.kind() {
        T![?] if config.exit_points && token.parent().and_then(ast::TryExpr::cast).is_some() => {
            highlight_exit_points(sema, token)
        }
        T![fn] | T![return] | T![->] if config.exit_points => highlight_exit_points(sema, token),
        T![await] | T![async] if config.yield_points => highlight_yield_points(token),
        T![for] if config.break_points && token.parent().and_then(ast::ForExpr::cast).is_some() => {
            highlight_break_points(token)
        }
        T![break] | T![loop] | T![while] | T![continue] if config.break_points => {
            highlight_break_points(token)
        }
        T![|] if config.closure_captures => highlight_closure_captures(sema, token, file_id),
        T![move] if config.closure_captures => highlight_closure_captures(sema, token, file_id),
        _ if config.references => highlight_references(sema, token, pos),
        _ => None,
    }
}

fn highlight_closure_captures(
    sema: &Semantics<'_, RootDatabase>,
    token: SyntaxToken,
    file_id: FileId,
) -> Option<Vec<HighlightedRange>> {
    let closure = token.parent_ancestors().take(2).find_map(ast::ClosureExpr::cast)?;
    let search_range = closure.body()?.syntax().text_range();
    let ty = &sema.type_of_expr(&closure.into())?.original;
    let c = ty.as_closure()?;
    Some(
        c.captured_items(sema.db)
            .into_iter()
            .map(|capture| capture.local())
            .flat_map(|local| {
                let usages = Definition::Local(local)
                    .usages(sema)
                    .in_scope(&SearchScope::file_range(FileRange { file_id, range: search_range }))
                    .include_self_refs()
                    .all()
                    .references
                    .remove(&file_id)
                    .into_iter()
                    .flatten()
                    .map(|FileReference { category, range, .. }| HighlightedRange {
                        range,
                        category,
                    });
                let category = local.is_mut(sema.db).then_some(ReferenceCategory::Write);
                local
                    .sources(sema.db)
                    .into_iter()
                    .flat_map(|x| x.to_nav(sema.db))
                    .filter(|decl| decl.file_id == file_id)
                    .filter_map(|decl| decl.focus_range)
                    .map(move |range| HighlightedRange { range, category })
                    .chain(usages)
            })
            .collect(),
    )
}

fn highlight_references(
    sema: &Semantics<'_, RootDatabase>,
    token: SyntaxToken,
    FilePosition { file_id, offset }: FilePosition,
) -> Option<Vec<HighlightedRange>> {
    let defs = if let Some((range, resolution)) =
        sema.check_for_format_args_template(token.clone(), offset)
    {
        match resolution.map(Definition::from) {
            Some(def) => iter::once(def).collect(),
            None => return Some(vec![HighlightedRange { range, category: None }]),
        }
    } else {
        find_defs(sema, token.clone())
    };
    let usages = defs
        .iter()
        .filter_map(|&d| {
            d.usages(sema)
                .in_scope(&SearchScope::single_file(file_id))
                .include_self_refs()
                .all()
                .references
                .remove(&file_id)
        })
        .flatten()
        .map(|FileReference { category, range, .. }| HighlightedRange { range, category });
    let mut res = FxHashSet::default();
    for &def in &defs {
        // highlight trait usages
        if let Definition::Trait(t) = def {
            let trait_item_use_scope = (|| {
                let name_ref = token.parent().and_then(ast::NameRef::cast)?;
                let path = full_path_of_name_ref(&name_ref)?;
                let parent = path.syntax().parent()?;
                match_ast! {
                    match parent {
                        ast::UseTree(it) => it.syntax().ancestors().find(|it| {
                            ast::SourceFile::can_cast(it.kind()) || ast::Module::can_cast(it.kind())
                        }),
                        ast::PathType(it) => it
                            .syntax()
                            .ancestors()
                            .nth(2)
                            .and_then(ast::TypeBoundList::cast)?
                            .syntax()
                            .parent()
                            .filter(|it| ast::WhereClause::can_cast(it.kind()) || ast::TypeParam::can_cast(it.kind()))?
                            .ancestors()
                            .find(|it| {
                                ast::Item::can_cast(it.kind())
                            }),
                        _ => None,
                    }
                }
            })();
            if let Some(trait_item_use_scope) = trait_item_use_scope {
                res.extend(
                    t.items_with_supertraits(sema.db)
                        .into_iter()
                        .filter_map(|item| {
                            Definition::from(item)
                                .usages(sema)
                                .set_scope(Some(&SearchScope::file_range(FileRange {
                                    file_id,
                                    range: trait_item_use_scope.text_range(),
                                })))
                                .include_self_refs()
                                .all()
                                .references
                                .remove(&file_id)
                        })
                        .flatten()
                        .map(|FileReference { category, range, .. }| HighlightedRange {
                            range,
                            category,
                        }),
                );
            }
        }

        // highlight the defs themselves
        match def {
            Definition::Local(local) => {
                let category = local.is_mut(sema.db).then_some(ReferenceCategory::Write);
                local
                    .sources(sema.db)
                    .into_iter()
                    .flat_map(|x| x.to_nav(sema.db))
                    .filter(|decl| decl.file_id == file_id)
                    .filter_map(|decl| decl.focus_range)
                    .map(|range| HighlightedRange { range, category })
                    .for_each(|x| {
                        res.insert(x);
                    });
            }
            def => {
                let navs = match def {
                    Definition::Module(module) => {
                        NavigationTarget::from_module_to_decl(sema.db, module)
                    }
                    def => match def.try_to_nav(sema.db) {
                        Some(it) => it,
                        None => continue,
                    },
                };
                for nav in navs {
                    if nav.file_id != file_id {
                        continue;
                    }
                    let hl_range = nav.focus_range.map(|range| {
                        let category = matches!(def, Definition::Local(l) if l.is_mut(sema.db))
                            .then_some(ReferenceCategory::Write);
                        HighlightedRange { range, category }
                    });
                    if let Some(hl_range) = hl_range {
                        res.insert(hl_range);
                    }
                }
            }
        }
    }

    res.extend(usages);
    if res.is_empty() {
        None
    } else {
        Some(res.into_iter().collect())
    }
}

fn highlight_exit_points(
    sema: &Semantics<'_, RootDatabase>,
    token: SyntaxToken,
) -> Option<Vec<HighlightedRange>> {
    fn hl(
        sema: &Semantics<'_, RootDatabase>,
        def_ranges: [Option<TextRange>; 2],
        body: Option<ast::Expr>,
    ) -> Option<Vec<HighlightedRange>> {
        let mut highlights = Vec::new();
        highlights.extend(
            def_ranges
                .into_iter()
                .flatten()
                .map(|range| HighlightedRange { category: None, range }),
        );
        let body = body?;
        walk_expr(&body, &mut |expr| match expr {
            ast::Expr::ReturnExpr(expr) => {
                if let Some(token) = expr.return_token() {
                    highlights.push(HighlightedRange { category: None, range: token.text_range() });
                }
            }
            ast::Expr::TryExpr(try_) => {
                if let Some(token) = try_.question_mark_token() {
                    highlights.push(HighlightedRange { category: None, range: token.text_range() });
                }
            }
            ast::Expr::MethodCallExpr(_) | ast::Expr::CallExpr(_) | ast::Expr::MacroExpr(_) => {
                if sema.type_of_expr(&expr).map_or(false, |ty| ty.original.is_never()) {
                    highlights.push(HighlightedRange {
                        category: None,
                        range: expr.syntax().text_range(),
                    });
                }
            }
            _ => (),
        });
        let tail = match body {
            ast::Expr::BlockExpr(b) => b.tail_expr(),
            e => Some(e),
        };

        if let Some(tail) = tail {
            for_each_tail_expr(&tail, &mut |tail| {
                let range = match tail {
                    ast::Expr::BreakExpr(b) => b
                        .break_token()
                        .map_or_else(|| tail.syntax().text_range(), |tok| tok.text_range()),
                    _ => tail.syntax().text_range(),
                };
                highlights.push(HighlightedRange { category: None, range })
            });
        }
        Some(highlights)
    }
    for anc in token.parent_ancestors() {
        return match_ast! {
            match anc {
                ast::Fn(fn_) => hl(sema, [fn_.fn_token().map(|it| it.text_range()), None], fn_.body().map(ast::Expr::BlockExpr)),
                ast::ClosureExpr(closure) => hl(
                    sema,
                    closure.param_list().map_or([None; 2], |p| [p.l_paren_token().map(|it| it.text_range()), p.r_paren_token().map(|it| it.text_range())]),
                    closure.body()
                ),
                ast::BlockExpr(block_expr) => if matches!(block_expr.modifier(), Some(ast::BlockModifier::Async(_) | ast::BlockModifier::Try(_)| ast::BlockModifier::Const(_))) {
                    hl(
                        sema,
                        [block_expr.modifier().and_then(|modifier| match modifier {
                            ast::BlockModifier::Async(t) | ast::BlockModifier::Try(t) | ast::BlockModifier::Const(t) => Some(t.text_range()),
                            _ => None,
                        }), None],
                        Some(block_expr.into())
                    )
                } else {
                    continue;
                },
                _ => continue,
            }
        };
    }
    None
}

fn highlight_break_points(token: SyntaxToken) -> Option<Vec<HighlightedRange>> {
    fn hl(
        cursor_token_kind: SyntaxKind,
        token: Option<SyntaxToken>,
        label: Option<ast::Label>,
        body: Option<ast::StmtList>,
    ) -> Option<Vec<HighlightedRange>> {
        let mut highlights = Vec::new();
        let range = cover_range(
            token.map(|tok| tok.text_range()),
            label.as_ref().map(|it| it.syntax().text_range()),
        );
        highlights.extend(range.map(|range| HighlightedRange { category: None, range }));
        for_each_break_and_continue_expr(label, body, &mut |expr| {
            let range: Option<TextRange> = match (cursor_token_kind, expr) {
                (T![for] | T![while] | T![loop] | T![break], ast::Expr::BreakExpr(break_)) => {
                    cover_range(
                        break_.break_token().map(|it| it.text_range()),
                        break_.lifetime().map(|it| it.syntax().text_range()),
                    )
                }
                (
                    T![for] | T![while] | T![loop] | T![continue],
                    ast::Expr::ContinueExpr(continue_),
                ) => cover_range(
                    continue_.continue_token().map(|it| it.text_range()),
                    continue_.lifetime().map(|it| it.syntax().text_range()),
                ),
                _ => None,
            };
            highlights.extend(range.map(|range| HighlightedRange { category: None, range }));
        });
        Some(highlights)
    }
    let parent = token.parent()?;
    let lbl = match_ast! {
        match parent {
            ast::BreakExpr(b) => b.lifetime(),
            ast::ContinueExpr(c) => c.lifetime(),
            ast::LoopExpr(l) => l.label().and_then(|it| it.lifetime()),
            ast::ForExpr(f) => f.label().and_then(|it| it.lifetime()),
            ast::WhileExpr(w) => w.label().and_then(|it| it.lifetime()),
            ast::BlockExpr(b) => Some(b.label().and_then(|it| it.lifetime())?),
            _ => return None,
        }
    };
    let lbl = lbl.as_ref();
    let label_matches = |def_lbl: Option<ast::Label>| match lbl {
        Some(lbl) => {
            Some(lbl.text()) == def_lbl.and_then(|it| it.lifetime()).as_ref().map(|it| it.text())
        }
        None => true,
    };
    let token_kind = token.kind();
    for anc in token.parent_ancestors().flat_map(ast::Expr::cast) {
        return match anc {
            ast::Expr::LoopExpr(l) if label_matches(l.label()) => hl(
                token_kind,
                l.loop_token(),
                l.label(),
                l.loop_body().and_then(|it| it.stmt_list()),
            ),
            ast::Expr::ForExpr(f) if label_matches(f.label()) => hl(
                token_kind,
                f.for_token(),
                f.label(),
                f.loop_body().and_then(|it| it.stmt_list()),
            ),
            ast::Expr::WhileExpr(w) if label_matches(w.label()) => hl(
                token_kind,
                w.while_token(),
                w.label(),
                w.loop_body().and_then(|it| it.stmt_list()),
            ),
            ast::Expr::BlockExpr(e) if e.label().is_some() && label_matches(e.label()) => {
                hl(token_kind, None, e.label(), e.stmt_list())
            }
            _ => continue,
        };
    }
    None
}

fn highlight_yield_points(token: SyntaxToken) -> Option<Vec<HighlightedRange>> {
    fn hl(
        async_token: Option<SyntaxToken>,
        body: Option<ast::Expr>,
    ) -> Option<Vec<HighlightedRange>> {
        let mut highlights =
            vec![HighlightedRange { category: None, range: async_token?.text_range() }];
        if let Some(body) = body {
            walk_expr(&body, &mut |expr| {
                if let ast::Expr::AwaitExpr(expr) = expr {
                    if let Some(token) = expr.await_token() {
                        highlights
                            .push(HighlightedRange { category: None, range: token.text_range() });
                    }
                }
            });
        }
        Some(highlights)
    }
    for anc in token.parent_ancestors() {
        return match_ast! {
            match anc {
                ast::Fn(fn_) => hl(fn_.async_token(), fn_.body().map(ast::Expr::BlockExpr)),
                ast::BlockExpr(block_expr) => {
                    if block_expr.async_token().is_none() {
                        continue;
                    }
                    hl(block_expr.async_token(), Some(block_expr.into()))
                },
                ast::ClosureExpr(closure) => hl(closure.async_token(), closure.body()),
                _ => continue,
            }
        };
    }
    None
}

fn cover_range(r0: Option<TextRange>, r1: Option<TextRange>) -> Option<TextRange> {
    match (r0, r1) {
        (Some(r0), Some(r1)) => Some(r0.cover(r1)),
        (Some(range), None) => Some(range),
        (None, Some(range)) => Some(range),
        (None, None) => None,
    }
}

fn find_defs(sema: &Semantics<'_, RootDatabase>, token: SyntaxToken) -> FxHashSet<Definition> {
    sema.descend_into_macros(DescendPreference::None, token)
        .into_iter()
        .filter_map(|token| IdentClass::classify_token(sema, &token))
        .map(IdentClass::definitions_no_ops)
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::fixture;

    use super::*;

    const ENABLED_CONFIG: HighlightRelatedConfig = HighlightRelatedConfig {
        break_points: true,
        exit_points: true,
        references: true,
        closure_captures: true,
        yield_points: true,
    };

    #[track_caller]
    fn check(ra_fixture: &str) {
        check_with_config(ra_fixture, ENABLED_CONFIG);
    }

    #[track_caller]
    fn check_with_config(ra_fixture: &str, config: HighlightRelatedConfig) {
        let (analysis, pos, annotations) = fixture::annotations(ra_fixture);

        let hls = analysis.highlight_related(config, pos).unwrap().unwrap_or_default();

        let mut expected = annotations
            .into_iter()
            .map(|(r, access)| (r.range, (!access.is_empty()).then_some(access)))
            .collect::<Vec<_>>();

        let mut actual = hls
            .into_iter()
            .map(|hl| {
                (
                    hl.range,
                    hl.category.map(|it| {
                        match it {
                            ReferenceCategory::Read => "read",
                            ReferenceCategory::Write => "write",
                            ReferenceCategory::Import => "import",
                        }
                        .to_string()
                    }),
                )
            })
            .collect::<Vec<_>>();
        actual.sort_by_key(|(range, _)| range.start());
        expected.sort_by_key(|(range, _)| range.start());

        assert_eq!(expected, actual);
    }

    #[test]
    fn test_hl_tuple_fields() {
        check(
            r#"
struct Tuple(u32, u32);

fn foo(t: Tuple) {
    t.0$0;
   // ^ read
    t.0;
   // ^ read
}
"#,
        );
    }

    #[test]
    fn test_hl_module() {
        check(
            r#"
//- /lib.rs
mod foo$0;
 // ^^^
//- /foo.rs
struct Foo;
"#,
        );
    }

    #[test]
    fn test_hl_self_in_crate_root() {
        check(
            r#"
use crate$0;
  //^^^^^ import
use self;
  //^^^^ import
mod __ {
    use super;
      //^^^^^ import
}
"#,
        );
        check(
            r#"
//- /main.rs crate:main deps:lib
use lib$0;
  //^^^ import
//- /lib.rs crate:lib
"#,
        );
    }

    #[test]
    fn test_hl_self_in_module() {
        check(
            r#"
//- /lib.rs
mod foo;
//- /foo.rs
use self$0;
 // ^^^^ import
"#,
        );
    }

    #[test]
    fn test_hl_local() {
        check(
            r#"
fn foo() {
    let mut bar = 3;
         // ^^^ write
    bar$0;
 // ^^^ read
}
"#,
        );
    }

    #[test]
    fn test_hl_local_in_attr() {
        check(
            r#"
//- proc_macros: identity
#[proc_macros::identity]
fn foo() {
    let mut bar = 3;
         // ^^^ write
    bar$0;
 // ^^^ read
}
"#,
        );
    }

    #[test]
    fn test_multi_macro_usage() {
        check(
            r#"
macro_rules! foo {
    ($ident:ident) => {
        fn $ident() -> $ident { loop {} }
        struct $ident;
    }
}

foo!(bar$0);
  // ^^^
fn foo() {
    let bar: bar = bar();
          // ^^^
                // ^^^
}
"#,
        );
        check(
            r#"
macro_rules! foo {
    ($ident:ident) => {
        fn $ident() -> $ident { loop {} }
        struct $ident;
    }
}

foo!(bar);
  // ^^^
fn foo() {
    let bar: bar$0 = bar();
          // ^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_yield_points() {
        check(
            r#"
pub async fn foo() {
 // ^^^^^
    let x = foo()
        .await$0
      // ^^^^^
        .await;
      // ^^^^^
    || { 0.await };
    (async { 0.await }).await
                     // ^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_yield_points2() {
        check(
            r#"
pub async$0 fn foo() {
 // ^^^^^
    let x = foo()
        .await
      // ^^^^^
        .await;
      // ^^^^^
    || { 0.await };
    (async { 0.await }).await
                     // ^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_let_else_yield_points() {
        check(
            r#"
pub async fn foo() {
 // ^^^^^
    let x = foo()
        .await$0
      // ^^^^^
        .await;
      // ^^^^^
    || { 0.await };
    let Some(_) = None else {
        foo().await
           // ^^^^^
    };
    (async { 0.await }).await
                     // ^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_yield_nested_fn() {
        check(
            r#"
async fn foo() {
    async fn foo2() {
 // ^^^^^
        async fn foo3() {
            0.await
        }
        0.await$0
       // ^^^^^
    }
    0.await
}
"#,
        );
    }

    #[test]
    fn test_hl_yield_nested_async_blocks() {
        check(
            r#"
async fn foo() {
    (async {
  // ^^^^^
        (async {
           0.await
        }).await$0 }
        // ^^^^^
    ).await;
}
"#,
        );
    }

    #[test]
    fn test_hl_exit_points() {
        check(
            r#"
  fn foo() -> u32 {
//^^
    if true {
        return$0 0;
     // ^^^^^^
    }

    0?;
  // ^
    0xDEAD_BEEF
 // ^^^^^^^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_exit_points2() {
        check(
            r#"
  fn foo() ->$0 u32 {
//^^
    if true {
        return 0;
     // ^^^^^^
    }

    0?;
  // ^
    0xDEAD_BEEF
 // ^^^^^^^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_exit_points3() {
        check(
            r#"
  fn$0 foo() -> u32 {
//^^
    if true {
        return 0;
     // ^^^^^^
    }

    0?;
  // ^
    0xDEAD_BEEF
 // ^^^^^^^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_let_else_exit_points() {
        check(
            r#"
  fn$0 foo() -> u32 {
//^^
    let Some(bar) = None else {
        return 0;
     // ^^^^^^
    };

    0?;
  // ^
    0xDEAD_BEEF
 // ^^^^^^^^^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_prefer_ref_over_tail_exit() {
        check(
            r#"
fn foo() -> u32 {
// ^^^
    if true {
        return 0;
    }

    0?;

    foo$0()
 // ^^^
}
"#,
        );
    }

    #[test]
    fn test_hl_never_call_is_exit_point() {
        check(
            r#"
struct Never;
impl Never {
    fn never(self) -> ! { loop {} }
}
macro_rules! never {
    () => { never() }
}
fn never() -> ! { loop {} }
  fn foo() ->$0 u32 {
//^^
    never();
 // ^^^^^^^
    never!();
 // ^^^^^^^^

    Never.never();
 // ^^^^^^^^^^^^^

    0
 // ^
}
"#,
        );
    }

    #[test]
    fn test_hl_inner_tail_exit_points() {
        check(
            r#"
  fn foo() ->$0 u32 {
//^^
    if true {
        unsafe {
            return 5;
         // ^^^^^^
            5
         // ^
        }
    } else if false {
        0
     // ^
    } else {
        match 5 {
            6 => 100,
              // ^^^
            7 => loop {
                break 5;
             // ^^^^^
            }
            8 => 'a: loop {
                'b: loop {
                    break 'a 5;
                 // ^^^^^
                    break 'b 5;
                    break 5;
                };
            }
            //
            _ => 500,
              // ^^^
        }
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_inner_tail_exit_points_labeled_block() {
        check(
            r#"
  fn foo() ->$0 u32 {
//^^
    'foo: {
        break 'foo 0;
     // ^^^^^
        loop {
            break;
            break 'foo 0;
         // ^^^^^
        }
        0
     // ^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_inner_tail_exit_points_loops() {
        check(
            r#"
  fn foo() ->$0 u32 {
//^^
    'foo: while { return 0; true } {
               // ^^^^^^
        break 'foo 0;
     // ^^^^^
        return 0;
     // ^^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_loop() {
        check(
            r#"
fn foo() {
    'outer: loop {
 // ^^^^^^^^^^^^
         break;
      // ^^^^^
         'inner: loop {
            break;
            'innermost: loop {
                break 'outer;
             // ^^^^^^^^^^^^
                break 'inner;
            }
            break$0 'outer;
         // ^^^^^^^^^^^^
            break;
        }
        break;
     // ^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_loop2() {
        check(
            r#"
fn foo() {
    'outer: loop {
        break;
        'inner: loop {
     // ^^^^^^^^^^^^
            break;
         // ^^^^^
            'innermost: loop {
                break 'outer;
                break 'inner;
             // ^^^^^^^^^^^^
            }
            break 'outer;
            break$0;
         // ^^^^^
        }
        break;
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_for() {
        check(
            r#"
fn foo() {
    'outer: for _ in () {
 // ^^^^^^^^^^^
         break;
      // ^^^^^
         'inner: for _ in () {
            break;
            'innermost: for _ in () {
                break 'outer;
             // ^^^^^^^^^^^^
                break 'inner;
            }
            break$0 'outer;
         // ^^^^^^^^^^^^
            break;
        }
        break;
     // ^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_for_but_not_continue() {
        check(
            r#"
fn foo() {
    'outer: for _ in () {
 // ^^^^^^^^^^^
        break;
     // ^^^^^
        continue;
        'inner: for _ in () {
            break;
            continue;
            'innermost: for _ in () {
                continue 'outer;
                break 'outer;
             // ^^^^^^^^^^^^
                continue 'inner;
                break 'inner;
            }
            break$0 'outer;
         // ^^^^^^^^^^^^
            continue 'outer;
            break;
            continue;
        }
        break;
     // ^^^^^
        continue;
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_continue_for_but_not_break() {
        check(
            r#"
fn foo() {
    'outer: for _ in () {
 // ^^^^^^^^^^^
        break;
        continue;
     // ^^^^^^^^
        'inner: for _ in () {
            break;
            continue;
            'innermost: for _ in () {
                continue 'outer;
             // ^^^^^^^^^^^^^^^
                break 'outer;
                continue 'inner;
                break 'inner;
            }
            break 'outer;
            continue$0 'outer;
         // ^^^^^^^^^^^^^^^
            break;
            continue;
        }
        break;
        continue;
     // ^^^^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_and_continue() {
        check(
            r#"
fn foo() {
    'outer: fo$0r _ in () {
 // ^^^^^^^^^^^
        break;
     // ^^^^^
        continue;
     // ^^^^^^^^
        'inner: for _ in () {
            break;
            continue;
            'innermost: for _ in () {
                continue 'outer;
             // ^^^^^^^^^^^^^^^
                break 'outer;
             // ^^^^^^^^^^^^
                continue 'inner;
                break 'inner;
            }
            break 'outer;
         // ^^^^^^^^^^^^
            continue 'outer;
         // ^^^^^^^^^^^^^^^
            break;
            continue;
        }
        break;
     // ^^^^^
        continue;
     // ^^^^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_while() {
        check(
            r#"
fn foo() {
    'outer: while true {
 // ^^^^^^^^^^^^^
         break;
      // ^^^^^
         'inner: while true {
            break;
            'innermost: while true {
                break 'outer;
             // ^^^^^^^^^^^^
                break 'inner;
            }
            break$0 'outer;
         // ^^^^^^^^^^^^
            break;
        }
        break;
     // ^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_labeled_block() {
        check(
            r#"
fn foo() {
    'outer: {
 // ^^^^^^^
         break;
      // ^^^^^
         'inner: {
            break;
            'innermost: {
                break 'outer;
             // ^^^^^^^^^^^^
                break 'inner;
            }
            break$0 'outer;
         // ^^^^^^^^^^^^
            break;
        }
        break;
     // ^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_unlabeled_loop() {
        check(
            r#"
fn foo() {
    loop {
 // ^^^^
        break$0;
     // ^^^^^
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_break_unlabeled_block_in_loop() {
        check(
            r#"
fn foo() {
    loop {
 // ^^^^
        {
            break$0;
         // ^^^^^
        }
    }
}
"#,
        );
    }

    #[test]
    fn test_hl_field_shorthand() {
        check(
            r#"
struct Struct { field: u32 }
              //^^^^^
fn function(field: u32) {
          //^^^^^
    Struct { field$0 }
           //^^^^^ read
}
"#,
        );
    }

    #[test]
    fn test_hl_disabled_ref_local() {
        let config = HighlightRelatedConfig { references: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
fn foo() {
    let x$0 = 5;
    let y = x * 2;
}
"#,
            config,
        );
    }

    #[test]
    fn test_hl_disabled_ref_local_preserved_break() {
        let config = HighlightRelatedConfig { references: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
fn foo() {
    let x$0 = 5;
    let y = x * 2;

    loop {
        break;
    }
}
"#,
            config.clone(),
        );

        check_with_config(
            r#"
fn foo() {
    let x = 5;
    let y = x * 2;

    loop$0 {
//  ^^^^
        break;
//      ^^^^^
    }
}
"#,
            config,
        );
    }

    #[test]
    fn test_hl_disabled_ref_local_preserved_yield() {
        let config = HighlightRelatedConfig { references: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
async fn foo() {
    let x$0 = 5;
    let y = x * 2;

    0.await;
}
"#,
            config.clone(),
        );

        check_with_config(
            r#"
    async fn foo() {
//  ^^^^^
        let x = 5;
        let y = x * 2;

        0.await$0;
//        ^^^^^
}
"#,
            config,
        );
    }

    #[test]
    fn test_hl_disabled_ref_local_preserved_exit() {
        let config = HighlightRelatedConfig { references: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
fn foo() -> i32 {
    let x$0 = 5;
    let y = x * 2;

    if true {
        return y;
    }

    0?
}
"#,
            config.clone(),
        );

        check_with_config(
            r#"
  fn foo() ->$0 i32 {
//^^
    let x = 5;
    let y = x * 2;

    if true {
        return y;
//      ^^^^^^
    }

    0?
//   ^
"#,
            config,
        );
    }

    #[test]
    fn test_hl_disabled_break() {
        let config = HighlightRelatedConfig { break_points: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
fn foo() {
    loop {
        break$0;
    }
}
"#,
            config,
        );
    }

    #[test]
    fn test_hl_disabled_yield() {
        let config = HighlightRelatedConfig { yield_points: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
async$0 fn foo() {
    0.await;
}
"#,
            config,
        );
    }

    #[test]
    fn test_hl_disabled_exit() {
        let config = HighlightRelatedConfig { exit_points: false, ..ENABLED_CONFIG };

        check_with_config(
            r#"
fn foo() ->$0 i32 {
    if true {
        return -1;
    }

    42
}"#,
            config,
        );
    }

    #[test]
    fn test_hl_multi_local() {
        check(
            r#"
fn foo((
    foo$0
  //^^^
    | foo
    //^^^
    | foo
    //^^^
): ()) {
    foo;
  //^^^read
    let foo;
}
"#,
        );
        check(
            r#"
fn foo((
    foo
  //^^^
    | foo$0
    //^^^
    | foo
    //^^^
): ()) {
    foo;
  //^^^read
    let foo;
}
"#,
        );
        check(
            r#"
fn foo((
    foo
  //^^^
    | foo
    //^^^
    | foo
    //^^^
): ()) {
    foo$0;
  //^^^read
    let foo;
}
"#,
        );
    }

    #[test]
    fn test_hl_trait_impl_methods() {
        check(
            r#"
trait Trait {
    fn func$0(self) {}
     //^^^^
}

impl Trait for () {
    fn func(self) {}
     //^^^^
}

fn main() {
    <()>::func(());
        //^^^^
    ().func();
     //^^^^
}
"#,
        );
        check(
            r#"
trait Trait {
    fn func(self) {}
}

impl Trait for () {
    fn func$0(self) {}
     //^^^^
}

fn main() {
    <()>::func(());
        //^^^^
    ().func();
     //^^^^
}
"#,
        );
        check(
            r#"
trait Trait {
    fn func(self) {}
}

impl Trait for () {
    fn func(self) {}
     //^^^^
}

fn main() {
    <()>::func(());
        //^^^^
    ().func$0();
     //^^^^
}
"#,
        );
    }

    #[test]
    fn test_assoc_type_highlighting() {
        check(
            r#"
trait Trait {
    type Output;
      // ^^^^^^
}
impl Trait for () {
    type Output$0 = ();
      // ^^^^^^
}
"#,
        );
    }

    #[test]
    fn test_closure_capture_pipe() {
        check(
            r#"
fn f() {
    let x = 1;
    //  ^
    let c = $0|y| x + y;
    //          ^ read
}
"#,
        );
    }

    #[test]
    fn test_closure_capture_move() {
        check(
            r#"
fn f() {
    let x = 1;
    //  ^
    let c = move$0 |y| x + y;
    //               ^ read
}
"#,
        );
    }

    #[test]
    fn test_trait_highlights_assoc_item_uses() {
        check(
            r#"
trait Foo {
    //^^^
    type T;
    const C: usize;
    fn f() {}
    fn m(&self) {}
}
impl Foo for i32 {
   //^^^
    type T = i32;
    const C: usize = 0;
    fn f() {}
    fn m(&self) {}
}
fn f<T: Foo$0>(t: T) {
      //^^^
    let _: T::T;
            //^
    t.m();
    //^
    T::C;
     //^
    T::f();
     //^
}

fn f2<T: Foo>(t: T) {
       //^^^
    let _: T::T;
    t.m();
    T::C;
    T::f();
}
"#,
        );
    }

    #[test]
    fn implicit_format_args() {
        check(
            r#"
//- minicore: fmt
fn test() {
    let a = "foo";
     // ^
    format_args!("hello {a} {a$0} {}", a);
                      // ^read
                          // ^read
                                  // ^read
}
"#,
        );
    }
}
