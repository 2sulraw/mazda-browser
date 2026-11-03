//! ES module loader (`<script type="module">`, static `import`/`export`).
//!
//! A module body runs in its own child environment of the global scope, so
//! its top-level bindings never leak into the page (unlike a classic script).
//! Dependencies are fetched synchronously in document order and executed
//! before the importing body, which matches the spec's evaluation order for
//! acyclic graphs.
//!
//! shortcuts (all honest):
//! - exported bindings are a snapshot taken when the defining module
//!   finishes; a later mutation of an exported `let` is not observed by
//!   importers (the spec's live bindings would need reference bindings)
//! - a cyclic import receives the still-empty namespace of the module on
//!   the stack instead of hoisted bindings
//! - relative specifiers resolve against the importing module's URL; bare
//!   package names (`import x from "lodash"`) do not resolve and fail
//!   loudly rather than silently importing nothing
//! - module scripts run in document order together with classic scripts
//!   (both are already deferred in this engine; there is no parser task to
//!   interleave them with)
//! - `import.meta`, dynamic `import()` and import assertions are not
//!   implemented: `import()`/`import.meta` fail at runtime (so the rest of
//!   the script still runs), import assertions do not parse

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::interp;
use super::parser::{self, ExportKind, ImportBinding, Stmt};
use super::{Env, JsEngine, JsError, Val};

/// `exported name -> value` for one module.
pub(crate) type Exports = Rc<RefCell<HashMap<Rc<str>, Val>>>;

/// Per-page module registry (one namespace per absolute URL, loaded once).
#[derive(Default)]
pub struct ModuleLoader {
    cache: HashMap<String, Exports>,
    /// URLs whose body is currently executing (cycle detection)
    active: Vec<String>,
}

impl ModuleLoader {
    pub fn new() -> ModuleLoader {
        ModuleLoader::default()
    }

    /// Fetch `url` and run it as a module. Errors are strings: the script
    /// runtime logs them and the page survives.
    pub fn load(&mut self, e: &mut JsEngine, url: &str) -> Result<Exports, String> {
        if let Some(x) = self.cache.get(url) {
            return Ok(x.clone());
        }
        if self.active.iter().any(|u| u == url) {
            // cycle: hand back the (still empty) namespace of the module that
            // is on the stack rather than recursing forever
            let ns: Exports = Rc::new(RefCell::new(HashMap::new()));
            self.cache.insert(url.to_string(), ns.clone());
            return Ok(ns);
        }
        let (bytes, _) =
            crate::fetch(url).map_err(|err| format!("module fetch failed {url}: {err}"))?;
        let src = String::from_utf8_lossy(&bytes).into_owned();
        self.load_source(e, url, &src)
    }

    /// Execute already-fetched module source. `url` is the module's own URL
    /// and doubles as the base for its import specifiers.
    pub fn load_source(
        &mut self,
        e: &mut JsEngine,
        url: &str,
        src: &str,
    ) -> Result<Exports, String> {
        if let Some(x) = self.cache.get(url) {
            return Ok(x.clone());
        }
        let src_id = e.register_source(src);
        let program = parser::parse(src, src_id).map_err(|err| format!("{url}: {err}"))?;
        // each module body is its own execution turn (same budget reset as
        // `JsEngine::exec`), so a huge dependency graph is not starved by
        // whichever module ran first
        e.steps = 0;
        e.call_depth = 0;
        e.cur_src = src_id;
        e.err_located = false;
        let ns: Exports = Rc::new(RefCell::new(HashMap::new()));
        self.cache.insert(url.to_string(), ns.clone());
        self.active.push(url.to_string());
        let r = self.run_body(e, url, src, &program, &ns);
        self.active.pop();
        if r.is_err() {
            // a failed module must not stay cached as a half namespace
            self.cache.remove(url);
        }
        r?;
        Ok(ns)
    }

    fn run_body(
        &mut self,
        e: &mut JsEngine,
        url: &str,
        src: &str,
        program: &[Stmt],
        ns: &Exports,
    ) -> Result<(), String> {
        let env = Env::child(&e.global_env);
        interp::hoist(e, &env, program);

        // A runtime throw carries its own `(line, col)` from `finish_error`;
        // the module URL is all this frame adds.
        let named = |e: &mut JsEngine, err: JsError| -> String {
            format!("{url}: {}", e.finish_error(err, src))
        };

        // 1) resolve every import first (document order, depth first)
        for st in program {
            if let parser::StmtKind::Import { source, bindings } = &st.kind {
                let dep = self.load_dep(e, url, source)?;
                for b in bindings {
                    match b {
                        ImportBinding::Named { imported, local } => {
                            let v = dep.borrow().get(imported).cloned().ok_or_else(|| {
                                format!("{url}: `{imported}` is not exported by {source:?}")
                            })?;
                            env.declare(local.clone(), v, true);
                        }
                        ImportBinding::Namespace { local } => {
                            env.declare(local.clone(), namespace(&dep), true);
                        }
                    }
                }
            }
        }

        // 2) run the body, recording exports as the declarations happen
        for st in program {
            match &st.kind {
                parser::StmtKind::Import { .. } => {}
                parser::StmtKind::Export {
                    kind,
                    names,
                    source,
                    inner,
                } => match kind {
                    ExportKind::Decl | ExportKind::Default => {
                        if let Some(inner) = inner {
                            interp::eval_stmt(e, &env, inner).map_err(|err| named(e, err))?;
                        }
                        for (exported, local) in names {
                            let v = e.env_get(&env, local);
                            ns.borrow_mut().insert(exported.clone(), v);
                        }
                    }
                    ExportKind::Named => match source {
                        None => {
                            for (exported, local) in names {
                                let v = e.env_get(&env, local);
                                ns.borrow_mut().insert(exported.clone(), v);
                            }
                        }
                        Some(spec) => {
                            let dep = self.load_dep(e, url, spec)?;
                            for (exported, local) in names {
                                let v = dep.borrow().get(local).cloned().ok_or_else(|| {
                                    format!("{url}: `{local}` is not exported by {spec:?}")
                                })?;
                                ns.borrow_mut().insert(exported.clone(), v);
                            }
                        }
                    },
                    ExportKind::All => {
                        let spec = source
                            .as_ref()
                            .ok_or_else(|| format!("{url}: `export *` without a source"))?;
                        let dep = self.load_dep(e, url, spec)?;
                        let entries: Vec<(Rc<str>, Val)> = dep
                            .borrow()
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        for (k, v) in entries {
                            if k.as_ref() != "default" {
                                ns.borrow_mut().insert(k, v);
                            }
                        }
                    }
                },
                _ => {
                    interp::eval_stmt(e, &env, st).map_err(|err| named(e, err))?;
                }
            }
        }
        Ok(())
    }

    fn load_dep(
        &mut self,
        e: &mut JsEngine,
        base: &str,
        spec: &str,
    ) -> Result<Exports, String> {
        let url = resolve_specifier(base, spec)
            .ok_or_else(|| format!("cannot resolve module specifier {spec:?} from {base}"))?;
        self.load(e, &url)
    }
}

/// The object handed to `import * as ns`.
fn namespace(xs: &Exports) -> Val {
    let o = Val::new_object();
    if let Val::Obj(obj) = &o {
        let entries: Vec<(Rc<str>, Val)> = xs
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in entries {
            super::obj_set_own(obj, &k, v);
        }
    }
    o
}

/// Absolute specifiers are used as-is; everything else joins the importing
/// module's URL (a browser does the same; bare package names then fail).
fn resolve_specifier(base: &str, spec: &str) -> Option<String> {
    if let Ok(u) = url::Url::parse(spec) {
        return Some(u.to_string());
    }
    url::Url::parse(base)
        .ok()?
        .join(spec)
        .ok()
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::Val;

    /// seed the cache with a ready-made dependency so a test never touches
    /// the network (the loader treats it exactly like an already-loaded URL)
    fn seed(loader: &mut ModuleLoader, url: &str, pairs: &[(&str, Val)]) {
        let ns: Exports = Rc::new(RefCell::new(HashMap::new()));
        for (k, v) in pairs {
            ns.borrow_mut().insert(Rc::from(*k), v.clone());
        }
        loader.cache.insert(url.to_string(), ns);
    }

    fn get(xs: &Exports, name: &str) -> Val {
        xs.borrow()
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("no export `{name}`"))
    }

    fn num(xs: &Exports, name: &str) -> f64 {
        match get(xs, name) {
            Val::Num(n) => n,
            other => panic!("`{name}` is not a number: {other:?}"),
        }
    }

    fn call(mut e: JsEngine, xs: &Exports, name: &str) -> f64 {
        let f = get(xs, name);
        match e.call_function(&f, Val::Undefined, &[]).expect("call") {
            Val::Num(n) => n,
            other => panic!("`{name}()` did not return a number: {other:?}"),
        }
    }

    #[test]
    fn exports_values_default_and_functions() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "export const a = 1; export function f() { return 2 } export default 3;",
            )
            .expect("module runs");
        assert_eq!(num(&xs, "a"), 1.0);
        assert_eq!(num(&xs, "default"), 3.0);
        assert_eq!(call(e, &xs, "f"), 2.0);
    }

    #[test]
    fn imports_default_named_and_namespace() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        seed(
            &mut l,
            "https://dep.test/dep.js",
            &[("a", Val::Num(2.0)), ("default", Val::Num(5.0))],
        );
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "import d, { a as b } from \"https://dep.test/dep.js\";
                 import * as ns from \"https://dep.test/dep.js\";
                 export const sum = d + b + ns.a;",
            )
            .expect("module runs");
        assert_eq!(num(&xs, "sum"), 9.0);
    }

    #[test]
    fn module_scope_is_private() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "const hidden = 5; export const seen = hidden;",
            )
            .expect("module runs");
        assert_eq!(num(&xs, "seen"), 5.0);
        // neither the module-local nor the exported binding leaks to the page
        assert!(matches!(e.env_get(&e.global_env, "hidden"), Val::Undefined));
        assert!(matches!(e.env_get(&e.global_env, "seen"), Val::Undefined));
    }

    #[test]
    fn reexports_named_and_star() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        seed(&mut l, "https://dep.test/a.js", &[("x", Val::Num(3.0))]);
        seed(
            &mut l,
            "https://dep.test/b.js",
            &[("y", Val::Num(4.0)), ("default", Val::Num(9.0))],
        );
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "export { x as renamed } from \"https://dep.test/a.js\";
                 export * from \"https://dep.test/b.js\";",
            )
            .expect("module runs");
        assert_eq!(num(&xs, "renamed"), 3.0);
        assert_eq!(num(&xs, "y"), 4.0);
        // `export *` never re-exports `default`
        assert!(xs.borrow().get("default").is_none());
    }

    #[test]
    fn export_list_of_locals() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "const a = 1; let b = 2; export { a, b as bee, a as default };",
            )
            .expect("module runs");
        assert_eq!(num(&xs, "a"), 1.0);
        assert_eq!(num(&xs, "bee"), 2.0);
        assert_eq!(num(&xs, "default"), 1.0);
    }

    #[test]
    fn export_default_anonymous_function() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "export default function () { return 7 }",
            )
            .expect("module runs");
        assert_eq!(call(e, &xs, "default"), 7.0);
    }

    #[test]
    fn export_default_anonymous_class() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "export default class { static tag() { return 8 } }",
            )
            .expect("module runs");
        assert!(matches!(get(&xs, "default"), Val::Func(_)));
        let _ = &mut e;
    }

    #[test]
    fn failed_module_is_not_cached() {
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let before = l
            .load_source(&mut e, "https://mazda.test/bad.js", "export const a = ;")
            .is_err();
        assert!(before);
        assert!(!l.cache.contains_key("https://mazda.test/bad.js"));
    }
}
