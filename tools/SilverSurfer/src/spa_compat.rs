//! Embedder-side React / SPA compatibility for Servo 0.7.
//!
//! Goal: cover **100% of mitigations the embedder can ship** — prefs that
//! exist on the published crate, document-start polyfills for missing globals,
//! and host-gated recoveries (GitHub hero). Remaining Custom Elements / Shadow
//! DOM / Service Worker / streams gaps are Servo motor ceilings; see
//! `docs/silversurfer-react-spa.md` in the project store.
//!
//! Polyfills only fill gaps (never overwrite natives). Keep them small: large
//! shims fight CSP and ES modules.

/// Document-start shims for APIs React 18+/19 and modern SPAs assume.
pub const SPA_POLYFILL_USERSCRIPT: &str = r#"(function () {
  try {
    if (typeof globalThis === "undefined") {
      window.globalThis = window;
    }

    if (typeof queueMicrotask !== "function") {
      window.queueMicrotask = function (fn) {
        Promise.resolve().then(fn).catch(function (err) {
          setTimeout(function () { throw err; }, 0);
        });
      };
    }

    if (typeof reportError !== "function") {
      window.reportError = function (err) {
        try { console.error(err); } catch (e) {}
      };
    }

    if (typeof requestIdleCallback !== "function") {
      window.requestIdleCallback = function (cb, opts) {
        var start = Date.now();
        var timeout = opts && opts.timeout;
        var id = setTimeout(function () {
          cb({
            didTimeout: !!timeout && Date.now() - start >= timeout,
            timeRemaining: function () {
              return Math.max(0, 50 - (Date.now() - start));
            }
          });
        }, 1);
        return id;
      };
      window.cancelIdleCallback = function (id) { clearTimeout(id); };
    }

    if (typeof scheduler === "undefined") {
      window.scheduler = {
        postTask: function (cb, opts) {
          var delay = (opts && opts.delay) || 0;
          return new Promise(function (resolve, reject) {
            setTimeout(function () {
              try { resolve(cb()); } catch (e) { reject(e); }
            }, delay);
          });
        }
      };
    }

    function abortReason(reason) {
      if (reason !== undefined) return reason;
      try { return new DOMException("Aborted", "AbortError"); }
      catch (e) {
        var err = new Error("Aborted");
        err.name = "AbortError";
        return err;
      }
    }

    // Full AbortController when missing (GitHub / React Query classic crash).
    if (typeof AbortController === "undefined") {
      function Signal() {
        this.aborted = false;
        this.reason = undefined;
        this.onabort = null;
        this._listeners = [];
      }
      Signal.prototype.addEventListener = function (type, fn) {
        if (type === "abort" && typeof fn === "function") this._listeners.push(fn);
      };
      Signal.prototype.removeEventListener = function (type, fn) {
        if (type !== "abort") return;
        this._listeners = this._listeners.filter(function (l) { return l !== fn; });
      };
      Signal.prototype.dispatchEvent = function (ev) {
        if (!ev || ev.type !== "abort") return false;
        if (typeof this.onabort === "function") {
          try { this.onabort(ev); } catch (e) {}
        }
        for (var i = 0; i < this._listeners.length; i++) {
          try { this._listeners[i].call(this, ev); } catch (e) {}
        }
        return true;
      };
      Signal.prototype.throwIfAborted = function () {
        if (this.aborted) throw this.reason || abortReason();
      };
      if (typeof AbortSignal === "undefined") {
        window.AbortSignal = Signal;
      }
      window.AbortController = function () {
        this.signal = new Signal();
      };
      window.AbortController.prototype.abort = function (reason) {
        var s = this.signal;
        if (s.aborted) return;
        s.aborted = true;
        s.reason = abortReason(reason);
        s.dispatchEvent({ type: "abort", target: s });
      };
    }

    // Statics / throwIfAborted on a partial native AbortSignal (Servo mid-stack).
    if (typeof AbortSignal !== "undefined") {
      if (typeof AbortSignal.prototype.throwIfAborted !== "function") {
        AbortSignal.prototype.throwIfAborted = function () {
          if (this.aborted) throw this.reason || abortReason();
        };
      }
      if (typeof AbortSignal.abort !== "function") {
        AbortSignal.abort = function (reason) {
          var c = new AbortController();
          c.abort(reason);
          return c.signal;
        };
      }
      if (typeof AbortSignal.timeout !== "function") {
        AbortSignal.timeout = function (ms) {
          var c = new AbortController();
          setTimeout(function () {
            try { c.abort(abortReason()); } catch (e) {}
          }, ms);
          return c.signal;
        };
      }
      if (typeof AbortSignal.any !== "function") {
        AbortSignal.any = function (signals) {
          var c = new AbortController();
          var list = signals || [];
          function onAbort() { try { c.abort(this.reason); } catch (e) {} }
          for (var i = 0; i < list.length; i++) {
            var s = list[i];
            if (!s) continue;
            if (s.aborted) { c.abort(s.reason); break; }
            if (typeof s.addEventListener === "function") {
              s.addEventListener("abort", onAbort);
            }
          }
          return c.signal;
        };
      }
    }

    if (typeof structuredClone !== "function") {
      window.structuredClone = function (value) {
        return JSON.parse(JSON.stringify(value));
      };
    }

    // Servo 0.7 without the `webcrypto` feature exposes no `crypto` global at
    // all. GitHub's marketing bundles call `crypto.randomUUID` at boot.
    if (typeof crypto === "undefined") {
      try { window.crypto = {}; } catch (e) {}
    }
    if (typeof crypto !== "undefined") {
      if (typeof crypto.getRandomValues !== "function") {
        crypto.getRandomValues = function (arr) {
          for (var i = 0; i < arr.length; i++) arr[i] = (Math.random() * 256) | 0;
          return arr;
        };
      }
      if (typeof crypto.randomUUID !== "function") {
        crypto.randomUUID = function () {
          var bytes = new Uint8Array(16);
          crypto.getRandomValues(bytes);
          bytes[6] = (bytes[6] & 0x0f) | 0x40;
          bytes[8] = (bytes[8] & 0x3f) | 0x80;
          var hex = [];
          for (var j = 0; j < 16; j++) hex.push((bytes[j] + 256).toString(16).slice(1));
          return (
            hex.slice(0, 4).join("") + "-" +
            hex.slice(4, 6).join("") + "-" +
            hex.slice(6, 8).join("") + "-" +
            hex.slice(8, 10).join("") + "-" +
            hex.slice(10, 16).join("")
          );
        };
      }
    }

    if (typeof Object.hasOwn !== "function") {
      Object.hasOwn = function (obj, prop) {
        return Object.prototype.hasOwnProperty.call(obj, prop);
      };
    }

    if (!Array.prototype.at) {
      Array.prototype.at = function (n) {
        n = Math.trunc(n) || 0;
        if (n < 0) n += this.length;
        if (n < 0 || n >= this.length) return undefined;
        return this[n];
      };
    }

    if (!String.prototype.replaceAll) {
      String.prototype.replaceAll = function (search, replacement) {
        if (Object.prototype.toString.call(search) === "[object RegExp]") {
          if (!search.global) throw new TypeError("replaceAll with RegExp needs /g");
          return this.replace(search, replacement);
        }
        return this.split(String(search)).join(String(replacement));
      };
    }

    if (typeof Element !== "undefined" && !Element.prototype.replaceChildren) {
      Element.prototype.replaceChildren = function () {
        while (this.firstChild) this.removeChild(this.firstChild);
        for (var i = 0; i < arguments.length; i++) {
          var n = arguments[i];
          this.appendChild(typeof n === "string" ? document.createTextNode(n) : n);
        }
      };
    }

    // No-op View Transitions — React / Next feature-detect; missing throws break shells.
    if (typeof Document !== "undefined" && !Document.prototype.startViewTransition) {
      Document.prototype.startViewTransition = function (cb) {
        var update = typeof cb === "function" ? cb() : undefined;
        var done = Promise.resolve(update);
        return {
          ready: done,
          finished: done,
          updateCallbackDone: done,
          skipTransition: function () {}
        };
      };
    }

    // Minimal Custom Elements registry only when the engine exposes none.
    // Never replace a native registry (GitHub's react-app needs real reactions).
    if (typeof customElements === "undefined") {
      var registry = {};
      window.customElements = {
        define: function (name, ctor) {
          registry[String(name).toLowerCase()] = ctor;
        },
        get: function (name) {
          return registry[String(name).toLowerCase()];
        },
        whenDefined: function (name) {
          var c = registry[String(name).toLowerCase()];
          return c ? Promise.resolve(c) : Promise.resolve();
        },
        upgrade: function () {}
      };
    }

    // IntersectionObserver stub only if missing (pref usually supplies a real one).
    if (typeof IntersectionObserver === "undefined") {
      window.IntersectionObserver = function (cb) {
        this._cb = cb;
        this._targets = [];
      };
      window.IntersectionObserver.prototype.observe = function (el) {
        var self = this;
        this._targets.push(el);
        setTimeout(function () {
          try {
            self._cb([{
              target: el,
              isIntersecting: true,
              intersectionRatio: 1,
              time: Date.now(),
              boundingClientRect: el.getBoundingClientRect ? el.getBoundingClientRect() : {},
              intersectionRect: el.getBoundingClientRect ? el.getBoundingClientRect() : {},
              rootBounds: null
            }], self);
          } catch (e) {}
        }, 0);
      };
      window.IntersectionObserver.prototype.unobserve = function () {};
      window.IntersectionObserver.prototype.disconnect = function () { this._targets = []; };
    }

    if (typeof ResizeObserver === "undefined") {
      window.ResizeObserver = function (cb) { this._cb = cb; };
      window.ResizeObserver.prototype.observe = function (el) {
        var self = this;
        setTimeout(function () {
          try {
            var rect = el.getBoundingClientRect ? el.getBoundingClientRect() : { width: 0, height: 0 };
            self._cb([{ target: el, contentRect: rect }], self);
          } catch (e) {}
        }, 0);
      };
      window.ResizeObserver.prototype.unobserve = function () {};
      window.ResizeObserver.prototype.disconnect = function () {};
    }
  } catch (e) {}
})();"#;

/// Reveal GitHub SSR hero / marketing blocks stuck at opacity:0 / visibility:hidden.
///
/// Prefer a one-shot stylesheet over polling `style.opacity`. The old interval
/// fought GSAP's `fromTo` / `clearProps` and made hero text flicker
/// (appear → disappear → appear). Servo 0.7 also rejects
/// `(prefers-reduced-motion: reduce)` CSS media rules, so we force the JS
/// `matchMedia` path GSAP uses to skip straight to the end state.
pub const GITHUB_REVEAL_USERSCRIPT: &str = r#"(function () {
  try {
    var host = location.hostname;
    if (host !== "github.com" && host.slice(-11) !== ".github.com") return;
    if (window.__ssGithubReveal) return;
    window.__ssGithubReveal = true;

    // Force reduced-motion for GSAP only on github.com (Servo MQ parse fails).
    try {
      var nativeMM = window.matchMedia && window.matchMedia.bind(window);
      window.matchMedia = function (query) {
        var q = String(query);
        if (/prefers-reduced-motion/i.test(q)) {
          return {
            matches: true,
            media: q,
            onchange: null,
            addListener: function () {},
            removeListener: function () {},
            addEventListener: function () {},
            removeEventListener: function () {},
            dispatchEvent: function () { return false; }
          };
        }
        return nativeMM
          ? nativeMM(query)
          : {
              matches: false,
              media: q,
              onchange: null,
              addListener: function () {},
              removeListener: function () {},
              addEventListener: function () {},
              removeEventListener: function () {},
              dispatchEvent: function () { return false; }
            };
      };
    } catch (e) {}

    function injectSheet() {
      if (document.getElementById("ss-github-reveal")) return;
      var style = document.createElement("style");
      style.id = "ss-github-reveal";
      style.textContent = [
        ".Intro__content, .Intro__mona, [data-hpc],",
        "react-app .PrimerBrand__Hero-module__HeroContent,",
        "react-app [class*='Intro__'] {",
        "  opacity: 1 !important;",
        "  visibility: visible !important;",
        "}"
      ].join("\n");
      (document.head || document.documentElement).appendChild(style);
    }

    function clearInlineHide() {
      var nodes = document.querySelectorAll(
        ".Intro__content, .Intro__mona, [data-hpc], react-app [style*='opacity'], react-app [style*='visibility']"
      );
      for (var i = 0; i < nodes.length; i++) {
        var el = nodes[i];
        if (!el || !el.style) continue;
        // Drop inline opacity/visibility so the stylesheet wins without
        // repeatedly writing !important over GSAP's timeline.
        if (el.style.opacity === "0") el.style.removeProperty("opacity");
        if (el.style.visibility === "hidden") el.style.removeProperty("visibility");
      }
    }

    function reveal() {
      injectSheet();
      clearInlineHide();
    }

    reveal();
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", reveal);
    }
    window.addEventListener("load", function () {
      reveal();
      // One delayed pass after hydration — not a multi-second polling fight.
      setTimeout(reveal, 250);
      setTimeout(reveal, 1000);
    });
  } catch (e) {}
})();"#;

/// True for `github.com` and its subdomains (gist, etc.).
pub fn is_github_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com" || host.ends_with(".github.com")
}

/// Pref keys we flip for React/SPA (must exist on Servo 0.7 `Preferences`).
pub const REACT_PREF_FLAGS: &[&str] = &[
    "dom_intersection_observer_enabled",
    "dom_web_animations_enabled",
    "dom_fontface_enabled",
    "dom_adoptedstylesheet_enabled",
    "dom_indexeddb_enabled",
    "dom_async_clipboard_enabled",
    "dom_visual_viewport_enabled",
    "dom_entries_api_enabled",
    "dom_offscreen_canvas_enabled",
    "dom_storage_manager_api_enabled",
    "dom_composition_event_enabled",
    "dom_cookiestore_enabled",
    "dom_credential_management_enabled",
    "dom_exec_command_enabled",
    "dom_geolocation_enabled",
    "dom_notification_enabled",
    "dom_permissions_enabled",
    "dom_allow_preloading_module_descendants",
    "dom_canvas_capture_enabled",
    "dom_wakelock_enabled",
    "dom_webrtc_transceiver_enabled",
];

/// Scripts installed into Servo's shared `UserContentManager` (document-start).
pub fn userscripts() -> [&'static str; 2] {
    [SPA_POLYFILL_USERSCRIPT, GITHUB_REVEAL_USERSCRIPT]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_hosts() {
        assert!(is_github_host("github.com"));
        assert!(is_github_host("GitHub.com"));
        assert!(is_github_host("gist.github.com"));
        assert!(!is_github_host("github.io"));
        assert!(!is_github_host("example.com"));
        assert!(!is_github_host("notgithub.com"));
    }

    #[test]
    fn github_reveal_covers_opacity_and_visibility() {
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("github.com"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("opacity"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("visibility"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("Intro__content"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("ss-github-reveal"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("prefers-reduced-motion"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("__ssGithubReveal"));
        assert!(!GITHUB_REVEAL_USERSCRIPT.contains("setInterval"));
    }

    #[test]
    fn spa_polyfill_covers_missing_crypto_global() {
        assert!(SPA_POLYFILL_USERSCRIPT.contains("typeof crypto === \"undefined\""));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("getRandomValues"));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("randomUUID"));
    }

    #[test]
    fn spa_polyfill_covers_react_surface() {
        for needle in [
            "AbortController",
            "AbortSignal.abort",
            "AbortSignal.timeout",
            "AbortSignal.any",
            "requestIdleCallback",
            "queueMicrotask",
            "structuredClone",
            "randomUUID",
            "Object.hasOwn",
            "replaceAll",
            "replaceChildren",
            "startViewTransition",
            "customElements",
            "IntersectionObserver",
            "ResizeObserver",
            "scheduler",
            "reportError",
            "globalThis",
        ] {
            assert!(
                SPA_POLYFILL_USERSCRIPT.contains(needle),
                "polyfill missing {needle}"
            );
        }
        assert!(SPA_POLYFILL_USERSCRIPT.contains("typeof AbortController"));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("typeof customElements === \"undefined\""));
    }

    #[test]
    fn userscript_order_polyfill_then_github() {
        let scripts = userscripts();
        assert_eq!(scripts[0], SPA_POLYFILL_USERSCRIPT);
        assert_eq!(scripts[1], GITHUB_REVEAL_USERSCRIPT);
    }

    #[test]
    fn react_pref_list_is_nonempty_and_dom_scoped() {
        assert!(REACT_PREF_FLAGS.len() >= 20);
        for flag in REACT_PREF_FLAGS {
            assert!(
                flag.starts_with("dom_"),
                "unexpected non-dom pref in react list: {flag}"
            );
            assert!(
                !flag.contains("serviceworker"),
                "service workers must stay off"
            );
        }
    }
}
