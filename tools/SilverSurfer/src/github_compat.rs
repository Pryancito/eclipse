//! Embedder-side GitHub homepage mitigations.
//!
//! The marketing landing at `https://github.com/` SSR-renders the hero with
//! inline `opacity:0` and expects GSAP (in `landing-pages-*.js`) to fade it
//! in. When that JS path fails under Servo, the SSR header still paints and
//! the first viewport stays blank white. See
//! `docs/silversurfer-github-blank.md` in the project store.

/// Userscript injected via Servo's `UserContentManager`. No-ops off github.com.
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
    fn userscript_is_github_gated_and_forces_opacity() {
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("github.com"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("opacity"));
        assert!(GITHUB_REVEAL_USERSCRIPT.contains("setProperty"));
    }
}
