//! `/usr/local/bin/eclipse-language`: the UI language of the whole session.
//!
//! Split out of `desktop.rs` because the script is the longest of the three
//! boot settings and `desktop.rs` was the file growing around it.
//!
//! The vocabulary is `es|us|toggle`, the same two country names `eclipse-kbd`
//! uses for the keyboard layout, so one pair of words covers both switches and
//! the labwc menu can say `es/us` once and mean it. What lands in
//! `/etc/eclipse/locale` is the LANGUAGE tag -- `lang=es` / `lang=en` -- which
//! is what `LANG`, `LANGUAGE` and lunarbar's own string table are keyed by:
//! `us` is a country, `en` is its language, and the file holds the one the
//! rest of the system can act on. The script prints back the country name it
//! was given, so `eclipse-language` and `eclipse-kbd` answer in the same
//! words.
//!
//! `eclipse-locale` stays as a name: it is what `/etc/profile`, the labwc
//! menu and every machine already installed call. It is now a wrapper that
//! execs this one, so there is exactly one implementation.

use std::{fs, path::Path};

/// Write `eclipse-language`, its `eclipse-locale` alias, and the default
/// `/etc/eclipse/locale`.
///
/// `--boot` is what eclipse-init runs before the compositor: it resolves the
/// language from the kernel command line (`lang=`) or the file, applies it,
/// and skips the SIGHUP, because there is no compositor yet to reconfigure.
pub(super) fn write_eclipse_language(rootfs: &Path) {
    let localbin = rootfs.join("usr/local/bin");
    let _ = fs::create_dir_all(&localbin);
    let script = localbin.join("eclipse-language");
    fs::write(
        &script,
        b"#!/bin/sh\n\
          # Eclipse OS: UI language (not keyboard layout -- see eclipse-kbd).\n\
          # Spoken as es|us, like eclipse-kbd; stored as the language tag the\n\
          # rest of the system is keyed by (lang=es / lang=en).\n\
          CONF=/etc/eclipse/locale\n\
          ENVF=\"${HOME:-/root}/.config/labwc/environment\"\n\
          MENUDIR=\"${HOME:-/root}/.config/labwc\"\n\
          LOG=\"${HOME:-/root}/.eclipse-language.log\"\n\
          \n\
          # The country names this command speaks.\n\
          country_ok() {\n\
          \x20 case \"$1\" in es|us) return 0 ;; *) return 1 ;; esac\n\
          }\n\
          \n\
          # country -> language tag, and back. Everything the script writes is\n\
          # a tag; everything it prints is a country.\n\
          tag_for() {\n\
          \x20 case \"$1\" in us) echo en ;; *) echo es ;; esac\n\
          }\n\
          \n\
          country_for() {\n\
          \x20 case \"$1\" in en|en_US|EN|us|US) echo us ;; *) echo es ;; esac\n\
          }\n\
          \n\
          # Every spelling that may reach us: the two country names, the two\n\
          # language tags, and the POSIX locales a hand-edited file or a\n\
          # kernel command line can carry. Anything else is not a language we\n\
          # have, and the caller is told so rather than quietly served\n\
          # Spanish.\n\
          normalize() {\n\
          \x20 case \"$1\" in\n\
          \x20   es|ES|es_ES|es_ES.UTF-8) echo es ;;\n\
          \x20   us|US|en|EN|en_US|en_US.UTF-8|en_GB) echo us ;;\n\
          \x20   *) return 1 ;;\n\
          \x20 esac\n\
          }\n\
          \n\
          posix_for() {\n\
          \x20 case \"$1\" in us) echo en_US.UTF-8 ;; *) echo es_ES.UTF-8 ;; esac\n\
          }\n\
          \n\
          language_for() {\n\
          \x20 case \"$1\" in us) echo en ;; *) echo es:en ;; esac\n\
          }\n\
          \n\
          file_lang() {\n\
          \x20 [ -r \"$CONF\" ] || return\n\
          \x20 awk '\n\
          \x20   { sub(/\\r$/, \"\") }\n\
          \x20   /^[[:space:]]*#/ { next }\n\
          \x20   /^[[:space:]]*lang[[:space:]]*=/ {\n\
          \x20     sub(/^[^=]*=/, \"\"); gsub(/[[:space:]]/, \"\"); print; exit\n\
          \x20   }\n\
          \x20 ' \"$CONF\"\n\
          }\n\
          \n\
          cmdline_lang() {\n\
          \x20 [ -r /proc/cmdline ] || return\n\
          \x20 awk '{\n\
          \x20   n=split($0,a,/[: \\t]/)\n\
          \x20   for(i=1;i<=n;i++) if(a[i] ~ /^lang=/){ sub(/^lang=/,\"\",a[i]); print a[i]; exit }\n\
          \x20 }' /proc/cmdline\n\
          }\n\
          \n\
          current() {\n\
          \x20 f=$(normalize \"$(file_lang)\" 2>/dev/null) || f=\n\
          \x20 if country_ok \"$f\"; then echo \"$f\"; return; fi\n\
          \x20 echo es\n\
          }\n\
          \n\
          resolve_boot() {\n\
          \x20 c=$(normalize \"$(cmdline_lang)\" 2>/dev/null) || c=\n\
          \x20 if country_ok \"$c\"; then echo \"$c\"; return; fi\n\
          \x20 current\n\
          }\n\
          \n\
          # One writer at a time for $ENVF. The three boot steps that write it\n\
          # -- eclipse-kbd (XKB_DEFAULT_LAYOUT), eclipse-language (LANG) and\n\
          # eclipse-tz (TZ) -- are independent of each other, so eclipse-init\n\
          # runs them AT ONCE (init.serial_setup on the kernel command line is\n\
          # the way back to one after another). Three read-modify-writes of one\n\
          # file at once lose keys: each reads the old file and the last mv\n\
          # wins. A directory is the lock because mkdir either creates it or\n\
          # fails, atomically, on every filesystem this runs on -- no busybox\n\
          # applet needed, and nothing left behind by a crash but an empty\n\
          # directory.\n\
          env_lock() {\n\
          \x20 i=0\n\
          \x20 while ! mkdir \"$ENVF.lock\" 2>/dev/null; do\n\
          \x20   i=$((i+1))\n\
          \x20   # BOUNDED, and then it takes the lock anyway. A boot step that\n\
          \x20   # waits for ever on a stale lock directory is how one hanging\n\
          \x20   # script wedges a whole machine (the reason a oneshot has a\n\
          \x20   # timeout at all); losing one key on one boot is the cheaper\n\
          \x20   # failure by a wide margin.\n\
          \x20   if [ \"$i\" -ge 60 ]; then\n\
          \x20     rm -rf \"$ENVF.lock\" 2>/dev/null\n\
          \x20     mkdir \"$ENVF.lock\" 2>/dev/null\n\
          \x20     break\n\
          \x20   fi\n\
          \x20   sleep 0.05 2>/dev/null || sleep 1\n\
          \x20 done\n\
          }\n\
          \n\
          env_unlock() {\n\
          \x20 rmdir \"$ENVF.lock\" 2>/dev/null || rm -rf \"$ENVF.lock\" 2>/dev/null\n\
          }\n\
          \n\
          upsert_env() {\n\
          \x20 key=$1; val=$2\n\
          \x20 mkdir -p \"$(dirname \"$ENVF\")\"\n\
          \x20 env_lock\n\
          \x20 # The temporary carries the pid: a fixed \"$ENVF.new\" is a second\n\
          \x20 # collision between concurrent writers, and it lands inside the\n\
          \x20 # blind spot above if the lock is ever stolen.\n\
          \x20 tmp=\"$ENVF.$$\"\n\
          \x20 if [ -f \"$ENVF\" ]; then\n\
          \x20   grep -v \"^${key}=\" \"$ENVF\" > \"$tmp\" 2>/dev/null || :\n\
          \x20   echo \"${key}=$val\" >> \"$tmp\"\n\
          \x20   mv \"$tmp\" \"$ENVF\"\n\
          \x20 else\n\
          \x20   echo \"${key}=$val\" > \"$ENVF\"\n\
          \x20 fi\n\
          \x20 env_unlock\n\
          }\n\
          \n\
          apply() {\n\
          \x20 country=$1\n\
          \x20 boot=$2\n\
          \x20 tag=$(tag_for \"$country\")\n\
          \x20 mkdir -p /etc/eclipse\n\
          \x20 echo \"lang=$tag\" > \"$CONF\"\n\
          \x20 upsert_env LANG \"$(posix_for \"$country\")\"\n\
          \x20 upsert_env LANGUAGE \"$(language_for \"$country\")\"\n\
          # The menu files are named by LANGUAGE tag (menu.es.xml/menu.en.xml),\n\
          # because that is what they are: translations, not countries.\n\
          \x20 src=\"$MENUDIR/menu.$tag.xml\"\n\
          \x20 if [ -f \"$src\" ]; then\n\
          \x20   cp -f \"$src\" \"$MENUDIR/menu.xml\"\n\
          \x20 fi\n\
          \x20 if [ \"$boot\" != boot ]; then\n\
          \x20   if [ -n \"${LABWC_PID:-}\" ] && kill -0 \"$LABWC_PID\" 2>/dev/null; then\n\
          \x20     kill -HUP \"$LABWC_PID\" 2>>\"$LOG\" || true\n\
          \x20   elif [ -x /usr/bin/labwc ]; then\n\
          \x20     /usr/bin/labwc --reconfigure >>\"$LOG\" 2>&1 || true\n\
          \x20   fi\n\
          \x20 fi\n\
          \x20 echo \"[$(date '+%H:%M:%S')] lang=$tag ($country) boot=$boot\" >>\"$LOG\"\n\
          }\n\
          \n\
          usage() {\n\
          \x20 echo \"usage: eclipse-language [es|us|toggle|--boot]\" >&2\n\
          \x20 exit 2\n\
          }\n\
          \n\
          case \"${1:-}\" in\n\
          '' ) current; exit 0 ;;\n\
          -h|--help) usage ;;\n\
          --boot)\n\
          \x20 apply \"$(resolve_boot)\" boot\n\
          \x20 current\n\
          \x20 exit 0\n\
          \x20 ;;\n\
          toggle)\n\
          \x20 if [ \"$(current)\" = es ]; then next=us; else next=es; fi\n\
          \x20 apply \"$next\"\n\
          \x20 echo \"$next\"\n\
          \x20 ;;\n\
          *)\n\
          \x20 want=$(normalize \"$1\") || usage\n\
          \x20 apply \"$want\"\n\
          \x20 echo \"$want\"\n\
          \x20 ;;\n\
          esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // `eclipse-locale` is the name /etc/profile, the labwc menu and every
    // already-installed machine call. It stays, as one line that execs the
    // real script: two copies of this logic is how the two names drift apart.
    let alias = localbin.join("eclipse-locale");
    fs::write(
        &alias,
        b"#!/bin/sh\n\
          # Eclipse OS: the older name of eclipse-language. One implementation.\n\
          exec /usr/local/bin/eclipse-language \"$@\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&alias, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let etc = rootfs.join("etc/eclipse");
    let _ = fs::create_dir_all(&etc);
    fs::write(etc.join("locale"), b"lang=es\n").unwrap();
}
