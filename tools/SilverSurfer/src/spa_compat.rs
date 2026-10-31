//! Embedder-side React / SPA compatibility helpers for Servo 0.7.
//!
//! Servo alone cannot match Firefox on Custom Elements, Shadow DOM, service
//! workers, or a complete modern fetch/streams stack. What the embedder *can*
//! do cheaply:
//!
//! 1. Flip experimental DOM prefs that React/marketing bundles probe
//!    (`IntersectionObserver`, Web Animations, `FontFace`,
//!    `adoptedStyleSheets`, IndexedDB, …).
//! 2. Inject a tiny document-start polyfill for gaps that throw synchronously
//!    (`AbortController` / `AbortSignal`, `requestIdleCallback`,
//!    `queueMicrotask`).
//! 3. Site-specific recoveries — today: force `opacity:1` on github.com when
//!    the GSAP intro never runs (see project store
//!    `docs/silversurfer-github-blank.md`).
//!
//! Polyfills are deliberately small. Massive shims fight CSP and ES modules.

/// Minimal shims for APIs React 18+ and modern SPAs assume exist.
///
/// Only fills missing globals; never replaces a native implementation.
pub const SPA_POLYFILL_USERSCRIPT: &str = r#"(function () {
  try {
    if (typeof queueMicrotask !== "function") {
      window.queueMicrotask = function (fn) {
        Promise.resolve().then(fn).catch(function (err) {
          setTimeout(function () { throw err; }, 0);
        });
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
      window.cancelIdleCallback = function (id) {
        clearTimeout(id);
      };
    }

    // AbortController / AbortSignal: React Query, fetch wrappers, and many
    // GitHub assets throw "AbortSignal is not defined" under incomplete Servo.
    if (typeof AbortController === "undefined") {
      function abortReason(reason) {
        if (reason !== undefined) return reason;
        try { return new DOMException("Aborted", "AbortError"); }
        catch (e) {
          var err = new Error("Aborted");
          err.name = "AbortError";
          return err;
        }
      }
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
  } catch (e) {}
})();"#;

/// Userscript that reveals GitHub's SSR hero when GSAP never flips opacity.
/// No-ops off `github.com` / `*.github.com`.
pub const GITHUB_REVEAL_USERSCRIPT: &str = r#"(function () {
  try {
    var host = location.hostname;
    if (host !== "github.com" && host.slice(-11) !== ".github.com") return;
    function reveal() {
      var nodes = document.querySelectorAll("[style*='opacity']");
      for (var i = 0; i < nodes.length; i++) {
        var el = nodes[i];
        var op = el.style && el.style.opacity;
        if (op === "0") el.style.setProperty("opacity", "1", "important");
      }
    }
    reveal();
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", reveal);
    }
    window.addEventListener("load", function () {
      reveal();
      var n = 0;
      var id = setInterval(function () {
        reveal();
        if (++n >= 10) clearInterval(id);
      }, 500);
    });
  } catch (e) {}
})();"#;

/// True for `github.com` and its subdomains (gist, etc.).
pub fn is_github_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "github.com" || host.ends_with(".github.com")
}

/// Scripts installed into Servo's shared `UserContentManager` (document-start).
/// Order: polyfills first, then site recoveries.
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
    fn github_reveal_is_gated_and_forces_opacity() {
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("github.com"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("opacity"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("setProperty"));
    }

    #[test]
    fn spa_polyfill_covers_abort_and_idle() {
        assert!(SPA_POLYFILL_USERSCRIPT.contains("AbortController"));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("AbortSignal"));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("requestIdleCallback"));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("queueMicrotask"));
        // Only fill gaps — never overwrite natives blindly.
        assert!(SPA_POLYFILL_USERSCRIPT.contains("typeof AbortController"));
        assert!(SPA_POLYFILL_USERSCRIPT.contains("typeof requestIdleCallback"));
    }

    #[test]
    fn userscript_order_polyfill_then_github() {
        let scripts = userscripts();
        assert_eq!(scripts[0], SPA_POLYFILL_USERSCRIPT);
        assert_eq!(scripts[1], GITHUB_REVEAL_USERSCRIPT);
        assert!(scripts[0].contains("AbortController"));
        assert!(scripts[1].contains("github.com"));
    }
}
