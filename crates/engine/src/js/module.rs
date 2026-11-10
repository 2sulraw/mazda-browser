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
//! - dynamic `import()` is a real Promise: the global native resolves the
//!   specifier and queues a `Job::Import` microtask; the engine's microtask
//!   pump loads and executes the module, then settles the promise — so
//!   `.then` chains and `await import(...)` both work, with module jobs
//!   interleaved in microtask order rather than the spec's separate
//!   module-job queue
//! - the specifier base is the URL of the module whose code is running
//!   (`src_id → URL` table: a module-defined function called later, from
//!   classic script code, still resolves against its own module), or the
//!   page URL outside module code
//! - `import.meta.url` is supported on the same lookup (no `resolve()`);
//!   import assertions do not parse

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
    /// the page's `<script type="importmap">`, if any
    import_map: Option<ImportMap>,
}

/// The `imports` half of a page's `<script type="importmap">`.
///
/// A bare specifier (`"react"`) has no URL of its own: the map names it. Keys
/// match exactly first, then by the longest key ending in `/` (a prefix
/// mapping, `"lodash/"`). Only bare specifiers consult the map -- relative and
/// absolute specifiers resolve against the importing module's URL, which is
/// what every map in the wild relies on.
#[derive(Default)]
pub struct ImportMap {
    entries: Vec<(String, String)>,
}

impl ImportMap {
    /// Parse the script's JSON, resolving values against the document URL.
    /// `scopes` and integrity metadata are ignored; malformed entries are
    /// dropped (the spec warns and continues). `None` when nothing usable is
    /// left.
    pub fn parse(text: &str, base: &str) -> Option<ImportMap> {
        let doc = url::Url::parse(base).ok()?;
        let v = super::builtins::json_parse_value(text).ok()?;
        let Val::Obj(root) = v else { return None };
        let Val::Obj(imports) = super::obj_get_own(&root, "imports") else {
            return None;
        };
        let mut entries = Vec::new();
        for (k, v) in imports.borrow().props.iter() {
            let Val::Str(url) = v else { continue };
            let Ok(abs) = doc.join(url) else { continue };
            let abs = abs.to_string();
            // a prefix key must name a prefix, and a file key a file
            if k.ends_with('/') != abs.ends_with('/') {
                continue;
            }
            entries.push((k.to_string(), abs));
        }
        (!entries.is_empty()).then_some(ImportMap { entries })
    }

    /// Number of usable entries (for the `[js]` load line).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Resolve a bare specifier through the map.
    pub fn resolve(&self, spec: &str) -> Option<String> {
        if let Some((_, url)) = self.entries.iter().find(|(k, _)| k == spec) {
            return Some(url.clone());
        }
        let (key, url) = self
            .entries
            .iter()
            .filter(|(k, _)| k.ends_with('/') && spec.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())?;
        Some(format!("{url}{}", &spec[key.len()..]))
    }
}

impl ModuleLoader {
    pub fn new() -> ModuleLoader {
        ModuleLoader::default()
    }

    /// Install the page's import map (before any module resolves an import).
    pub fn set_import_map(&mut self, map: ImportMap) {
        self.import_map = Some(map);
    }

    /// `resolve_specifier`, falling back to the import map for a bare name.
    fn resolve(&self, base: &str, spec: &str) -> Option<String> {
        resolve_with_map(self.import_map.as_ref(), base, spec)
    }

    /// The page's import map, for the preloader.
    pub(crate) fn import_map(&self) -> Option<&ImportMap> {
        self.import_map.as_ref()
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
        if src_id != 0 {
            // remember which URL this source belongs to: `import()` and
            // `import.meta.url` resolve against it even when a module-defined
            // function is called later from classic script code
            e.module_urls.insert(src_id, Rc::from(url));
        }
        let program = parser::parse(src, src_id).map_err(|err| format!("{url}: {err}"))?;
        // each module body is its own execution turn (fresh step budget), but
        // it may run re-entrantly from a microtask inside an outer call
        // (`await import(...)`), so the caller's frame depth is restored after
        e.steps = 0;
        let saved_depth = e.call_depth;
        e.call_depth = 0;
        // the diagnostic cursor belongs to the calling turn; running a module
        // body must not leave `cur_src` pointing at this module afterwards
        // (that is what made a later page-script throw resolve its offset
        // against the module's text)
        let outer_src = e.cur_src;
        let outer_located = e.err_located;
        e.cur_src = src_id;
        e.err_located = false;
        let ns: Exports = Rc::new(RefCell::new(HashMap::new()));
        self.cache.insert(url.to_string(), ns.clone());
        self.active.push(url.to_string());
        let r = self.run_body(e, url, src, src_id, &program, &ns);
        self.active.pop();
        e.call_depth = saved_depth;
        e.cur_src = outer_src;
        e.err_located = outer_located;
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
        src_id: u32,
        program: &[Stmt],
        ns: &Exports,
    ) -> Result<(), String> {
        let env = Env::child(&e.global_env);
        interp::hoist(e, &env, program);

        // A runtime throw carries its own `(line, col)` from `finish_error`;
        // the module URL is all this frame adds.
        let named = |e: &mut JsEngine, err: JsError| -> String {
            // the `(line, col)` suffix resolves against the source of the
            // function that threw, which for a bundle chunk is NOT this
            // module — name that origin too when it differs
            let origin = if matches!(err, JsError::Throw(_)) {
                e.module_url_for(e.throw_src)
            } else {
                None
            };
            let located = e.finish_error(err, src);
            match origin {
                Some(o) if o.as_ref() != url => format!("{url} (throw in {o}): {located}"),
                _ => format!("{url}: {located}"),
            }
        };

        // 1) resolve every import first (document order, depth first)
        for st in program {
            if let parser::StmtKind::Import { source, bindings } = &st.kind {
                // loading a dependency runs another module body; come back
                // with THIS module's diagnostic cursor
                e.cur_src = src_id;
                e.err_located = false;
                let dep = self.load_dep(e, url, source)?;
                e.cur_src = src_id;
                for b in bindings {
                    match b {
                        ImportBinding::Named { imported, local } => {
                            let v = dep.borrow().get(imported).cloned().ok_or_else(|| {
                                format!("{url}: `{imported}` is not exported by {source:?}")
                            })?;
                            env.declare(local.clone(), v, true);
                        }
                        ImportBinding::Namespace { local } => {
                            env.declare(local.clone(), namespace_value(&dep), true);
                        }
                    }
                }
            }
        }

        // 2) run the body, recording exports as the declarations happen
        for st in program {
            // per-statement diagnostic origin: a caught/handled error in an
            // earlier statement must not label a later throw (and nested
            // module loads move `cur_src`)
            e.cur_src = src_id;
            e.err_located = false;
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
        let url = self
            .resolve(base, spec)
            .ok_or_else(|| format!("cannot resolve module specifier {spec:?} from {base}"))?;
        self.load(e, &url)
    }

    /// Test hook: register a module namespace without fetching/executing.
    /// Dynamic-import tests seed the target so they never touch the network
    /// (`load` treats a cached URL exactly like an already-evaluated one).
    #[cfg(test)]
    pub(crate) fn seed(&mut self, url: &str, pairs: &[(&str, Val)]) {
        let ns: Exports = Rc::new(RefCell::new(HashMap::new()));
        for (k, v) in pairs {
            ns.borrow_mut().insert(Rc::from(*k), v.clone());
        }
        self.cache.insert(url.to_string(), ns);
    }
}

/// The object handed to `import * as ns` (and to `import()`'s `.then`).
/// Live: the object reads the module's export map at access time, so a
/// namespace imported during a cycle sees exports the body records later
/// (an entry chunk importing itself calls the runtime with its own
/// namespace — rspack/webpack emit that shape).
pub(crate) fn namespace_value(xs: &Exports) -> Val {
    Val::Obj(Rc::new(RefCell::new(super::Obj {
        kind: super::ObjKind::ModuleNs(xs.clone()),
        props: Vec::new(),
        proto: None,
    })))
}

/// Absolute specifiers are used as-is, `./` `../` `/` join the importing
/// module's URL. A **bare** specifier (`"react"`, `"lodash/fp"`) is a package
/// name: only an import map can resolve it, so without one it fails *without a
/// request*. Letting `Url::join` have it would invent a 404 URL next to the
/// module (`/assets/react`), which is both wrong per spec and one wasted round
/// trip per import -- GitHub's bundle imports four such names, which cost
/// ~1.7 s of dead latency on a cold load.
fn resolve_specifier(base: &str, spec: &str) -> Option<String> {
    if let Ok(u) = url::Url::parse(spec) {
        return Some(u.to_string());
    }
    let relative = spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/');
    if !relative {
        return None; // bare: an import map's job (unsupported)
    }
    url::Url::parse(base)
        .ok()?
        .join(spec)
        .ok()
        .map(|u| u.to_string())
}

/// `resolve_specifier` plus the page's import map for a bare name. Shared by
/// the loader and the module-graph preloader so the two cannot disagree.
pub(crate) fn resolve_with_map(
    map: Option<&ImportMap>,
    base: &str,
    spec: &str,
) -> Option<String> {
    resolve_specifier(base, spec).or_else(|| map.and_then(|m| m.resolve(spec)))
}

/// Static import specifiers of a module body: `import ... from "x"` plus
/// `export ... from "x"`. Used only to warm the HTTP cache before execution;
/// a parse error yields no specifiers (the real load reports it).
pub(crate) fn static_deps(src: &str) -> Vec<String> {
    let Ok(program) = parser::parse(src, 0) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for st in &program {
        match &st.kind {
            parser::StmtKind::Import { source, .. } => out.push(source.to_string()),
            parser::StmtKind::Export {
                source: Some(s), ..
            } => out.push(s.to_string()),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Dynamic `import()` — a global native queues a `Job::Import` microtask and
// returns the pending Promise; the engine's microtask pump loads the module
// and settles it (so `.then` chains and `await import(...)` both work)
// ---------------------------------------------------------------------------

/// Install the global `import` native (call once per page, after
/// `JsEngine::new`). The page URL fallback comes from `JsEngine::import_base`.
pub(crate) fn install_dynamic_import(e: &mut JsEngine) {
    let id = e.register_native("import", n_dynamic_import, 0);
    e.set_global("import", Val::Native(id));
    e.import_native = Some(id);
}

/// `import(spec)`: ToString the argument, resolve it against the running
/// module's URL (or the page URL), queue the load as a microtask and return
/// a pending Promise. An unresolvable specifier still returns a Promise —
/// already rejected, so `try`/`catch` around `import()` sees no throw (the
/// spec rejects asynchronously; this engine cannot observe the difference
/// beyond microtask order).
fn n_dynamic_import(
    e: &mut JsEngine,
    _idx: u16,
    _this: &Val,
    args: &[Val],
) -> Result<Val, JsError> {
    let spec = args.first().map(|v| v.to_string()).unwrap_or_default();
    let base = e
        .module_url_for(e.cur_src)
        .or_else(|| e.import_base.clone())
        .unwrap_or_default();
    let p = e.new_promise();
    let resolved = match e.module_loader.as_ref() {
        Some(l) => l.resolve(&base, &spec),
        None => resolve_specifier(&base, &spec),
    };
    match resolved {
        Some(url) => e.microtasks.push(super::Job::Import {
            url: Rc::from(url.as_str()),
            promise: p.clone(),
        }),
        None => {
            let msg = format!(
                "TypeError: cannot resolve module specifier {spec:?} (base {base:?})"
            );
            eprintln!("[js] dynamic import failed: {msg}");
            super::settle_promise(e, &p, Err(Val::Str(Rc::from(msg.as_str()))));
        }
    }
    Ok(Val::Obj(p))
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

    #[test]
    fn import_meta_url_names_the_defining_module() {
        let mut e = JsEngine::new();
        install_dynamic_import(&mut e);
        e.import_base = Some(Rc::from("https://mazda.test/page/index.html"));
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/dir/main.js",
                "export function where() { return import.meta.url; }",
            )
            .expect("module runs");
        // called later, outside the module body: the closure's source id
        // still maps to its defining module
        let f = get(&xs, "where");
        let v = e.call_function(&f, Val::Undefined, &[]).expect("call");
        assert_eq!(v.to_string(), "https://mazda.test/dir/main.js");
    }

    #[test]
    fn self_import_namespace_is_live_during_the_body() {
        // rspack/webpack entry chunks import THEMSELVES and hand that
        // namespace to the runtime at the tail of the body (MDN's client
        // bundle). Imports resolve before the body runs, so a snapshot taken
        // then would be empty; the namespace must read the export map live.
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/entry.js",
                "export const ids = [8410];
                 import * as self_ns from './entry.js';
                 globalThis.seen = self_ns.ids.length;
                 globalThis.has_toString = 'toString' in self_ns;
                 export const ok = self_ns.ids[0] === 8410;",
            )
            .expect("module runs");
        assert_eq!(e.get_global("seen").to_string(), "1");
        assert!(matches!(get(&xs, "ok"), Val::Bool(true)));
        // a namespace has no prototype chain (no Object.prototype leak)
        assert!(matches!(e.get_global("has_toString"), Val::Bool(false)));
    }

    #[test]
    fn error_origin_names_the_defining_module_when_it_differs() {
        // a bundle chunk's function can throw while the ENTRY module is
        // evaluating; the log must name both, or the reported line/col looks
        // like it belongs to the wrong file
        let mut e = JsEngine::new();
        let mut l = ModuleLoader::new();
        l.load_source(
            &mut e,
            "https://mazda.test/dep.js",
            "export function boom(x) { return x.length; }",
        )
        .expect("dep runs");
        let err = l
            .load_source(
                &mut e,
                "https://mazda.test/main.js",
                "import { boom } from './dep.js'; boom(undefined);",
            )
            .expect_err("main throws");
        assert!(
            err.contains("throw in https://mazda.test/dep.js"),
            "origin named: {err}"
        );
        assert!(err.contains("line 1"), "location from dep: {err}");
    }

    #[test]
    fn dynamic_import_inside_module_function_uses_module_base() {
        let mut e = JsEngine::new();
        install_dynamic_import(&mut e);
        e.import_base = Some(Rc::from("https://mazda.test/page/index.html"));
        let mut l = ModuleLoader::new();
        l.seed("https://mazda.test/dir/dep.js", &[("v", Val::Num(3.0))]);
        let xs = l
            .load_source(
                &mut e,
                "https://mazda.test/dir/main.js",
                "export function load() {
                     return import(\"./dep.js\").then(function (ns) {
                         globalThis.late = ns.v;
                     });
                 }",
            )
            .expect("module runs");
        e.module_loader = Some(l);
        let f = get(&xs, "load");
        e.call_function(&f, Val::Undefined, &[]).expect("call");
        e.drain_microtasks().expect("microtasks");
        assert_eq!(e.get_global("late").to_string(), "3");
    }

    #[test]
    fn dynamic_import_resolves_from_page_base() {
        let mut e = JsEngine::new();
        install_dynamic_import(&mut e);
        e.import_base = Some(Rc::from("https://mazda.test/page/"));
        let mut l = ModuleLoader::new();
        l.seed("https://mazda.test/page/dep.js", &[("v", Val::Num(4.0))]);
        e.module_loader = Some(l);
        // `exec` drains microtasks at the end of the turn: the import job
        // loads the seeded module, settles, and `.then` runs
        e.exec(
            "import(\"./dep.js\").then(function (ns) { globalThis.got = ns.v; },
                function (err) { globalThis.err = err; });",
        )
        .expect("script runs");
        assert_eq!(e.get_global("got").to_string(), "4");
        assert!(matches!(e.get_global("err"), Val::Undefined));
    }

    #[test]
    fn dynamic_import_rejects_unresolvable_specifier() {
        let mut e = JsEngine::new();
        install_dynamic_import(&mut e);
        // no page base and no module URL: a relative specifier cannot resolve
        e.module_loader = Some(ModuleLoader::new());
        e.exec(
            "import(\"./nowhere.js\").then(function () { globalThis.ok = 1; },
                function (err) { globalThis.err = err; });",
        )
        .expect("script runs");
        assert!(matches!(e.get_global("ok"), Val::Undefined));
        let err = e.get_global("err").to_string();
        assert!(err.contains("cannot resolve module specifier"), "{err}");
    }

    #[test]
    fn import_map_resolves_bare_specifiers() {
        let map = ImportMap::parse(
            r#"{"imports":{
                "react":"https://cdn.test/react.js",
                "react/":"https://cdn.test/react/",
                "local":"./local.js",
                "bad-prefix":"https://cdn.test/x/",
                "bad-dir/":"https://cdn.test/y.js"
            }}"#,
            "https://mazda.test/page/index.html",
        )
        .expect("usable map");
        // the two malformed entries (file key -> prefix URL and the reverse)
        // are dropped
        assert_eq!(map.len(), 3);
        assert!(map.resolve("bad-prefix").is_none());
        assert!(map.resolve("bad-dir/x.js").is_none());
        assert_eq!(
            map.resolve("react").as_deref(),
            Some("https://cdn.test/react.js")
        );
        // prefix mapping keeps the tail of the specifier
        assert_eq!(
            map.resolve("react/jsx-runtime").as_deref(),
            Some("https://cdn.test/react/jsx-runtime")
        );
        // an exact key wins over a prefix
        assert_eq!(
            map.resolve("react/").as_deref(),
            Some("https://cdn.test/react/")
        );
        // values resolve against the document URL
        assert_eq!(
            map.resolve("local").as_deref(),
            Some("https://mazda.test/page/local.js")
        );
        assert_eq!(map.resolve("scheduler"), None);
        assert!(ImportMap::parse("{}", "https://mazda.test/").is_none());
        assert!(ImportMap::parse("not json", "https://mazda.test/").is_none());
    }

    #[test]
    fn loader_consults_the_map_only_for_bare_specifiers() {
        let mut l = ModuleLoader::new();
        l.set_import_map(
            ImportMap::parse(
                r#"{"imports":{"react":"https://cdn.test/react.js"}}"#,
                "https://mazda.test/page/",
            )
            .expect("map"),
        );
        let base = "https://mazda.test/assets/core.js";
        assert_eq!(
            l.resolve(base, "react").as_deref(),
            Some("https://cdn.test/react.js")
        );
        // a relative specifier still resolves against the module, and a bare
        // name the map does not know is still an error
        assert_eq!(
            l.resolve(base, "./chunk.js").as_deref(),
            Some("https://mazda.test/assets/chunk.js")
        );
        assert_eq!(l.resolve(base, "lodash"), None);
    }

    #[test]
    fn static_deps_lists_import_and_export_from_sources() {
        let src = r#"
            import def from "./a.js";
            import { x as y } from "react";
            import * as ns from "../b.js";
            import "./side-effect.js";
            export { z } from "./c.js";
            export * from "./d.js";
            export const own = 1;
            const later = () => import("./lazy.js");
        "#;
        // the preloader warms the static graph only: dynamic imports stay lazy
        assert_eq!(
            static_deps(src),
            vec![
                "./a.js".to_string(),
                "react".to_string(),
                "../b.js".to_string(),
                "./side-effect.js".to_string(),
                "./c.js".to_string(),
                "./d.js".to_string(),
            ]
        );
        // a body that does not parse has no trustworthy edges
        assert!(static_deps("import {").is_empty());
    }

    #[test]
    fn resolve_with_map_serves_the_preloader_the_same_urls() {
        let map = ImportMap::parse(
            r#"{"imports":{"react":"https://cdn.test/react.js"}}"#,
            "https://mazda.test/page/",
        )
        .expect("map");
        let base = "https://mazda.test/assets/core.js";
        assert_eq!(
            resolve_with_map(Some(&map), base, "react").as_deref(),
            Some("https://cdn.test/react.js")
        );
        assert_eq!(
            resolve_with_map(Some(&map), base, "./chunk.js").as_deref(),
            Some("https://mazda.test/assets/chunk.js")
        );
        // no map: a bare name has no URL, and the preloader must not invent one
        assert_eq!(resolve_with_map(None, base, "react"), None);
    }

    #[test]
    fn bare_specifiers_do_not_invent_a_url() {
        // `react` is a package name: without an import map it is a resolution
        // error, not a request to `/assets/react` (which 404s over the wire)
        assert!(resolve_specifier("https://mazda.test/assets/core.js", "react").is_none());
        assert!(resolve_specifier("https://mazda.test/assets/core.js", "lodash/fp").is_none());
        assert_eq!(
            resolve_specifier("https://mazda.test/assets/core.js", "./chunk.js").as_deref(),
            Some("https://mazda.test/assets/chunk.js")
        );
        assert_eq!(
            resolve_specifier("https://mazda.test/assets/core.js", "/x.js").as_deref(),
            Some("https://mazda.test/x.js")
        );
        assert_eq!(
            resolve_specifier("https://mazda.test/a/core.js", "https://cdn.test/m.js").as_deref(),
            Some("https://cdn.test/m.js")
        );
    }

    #[test]
    fn dynamic_import_specifier_is_module_relative() {
        let mut e = JsEngine::new();
        install_dynamic_import(&mut e);
        e.import_base = Some(Rc::from("https://mazda.test/page/index.html"));
        let mut l = ModuleLoader::new();
        l.seed("https://mazda.test/dir/dep.js", &[("v", Val::Num(7.0))]);
        e.module_loader = Some(l);
        e.load_module_source(
            "https://mazda.test/dir/main.js",
            "globalThis.pp = import(\"./dep.js\").then(function (ns) {
                 globalThis.got2 = ns.v;
             });",
        )
        .expect("module runs");
        e.drain_microtasks().expect("microtasks");
        // resolved against the module URL (`/dir/`), not the page URL
        assert_eq!(e.get_global("got2").to_string(), "7");
    }
}
