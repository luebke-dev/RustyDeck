//! Templating for labels, icons and colours, driven by a key's state.
//!
//! Anything a key shows may contain a [minijinja](https://docs.rs/minijinja)
//! expression, so the output of the state command can be shown as text or pick
//! an icon: `label: "Vol {{ stdout }}"`.

use minijinja::{Environment, context};
use std::sync::OnceLock;

/// Does this text hold a template at all? Plain strings skip the engine.
pub fn is_template(text: &str) -> bool {
    text.contains("{{") || text.contains("{%")
}

/// What a template can refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// Output of the state command, trimmed.
    pub stdout: String,
    /// Exit status of the state command.
    pub exit: Option<i32>,
    /// Index of the matching state case.
    pub case: usize,
    /// True until the first poll has answered. A template that cannot cope
    /// with the empty output yet is not worth a warning.
    pub pending: bool,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            stdout: String::new(),
            exit: None,
            case: 0,
            pending: true,
        }
    }
}

impl Context {
    pub fn from_output(stdout: &str, exit: Option<i32>, case: usize) -> Self {
        Self {
            stdout: stdout.trim().to_string(),
            exit,
            case,
            pending: false,
        }
    }
}

fn environment() -> &'static Environment<'static> {
    static ENV: OnceLock<Environment<'static>> = OnceLock::new();
    ENV.get_or_init(Environment::new)
}

/// Render one piece of text. A template that fails falls back to the text as
/// written, so a typo costs a label rather than the whole key.
pub fn render(text: &str, ctx: &Context) -> String {
    if !is_template(text) {
        return text.to_string();
    }

    let lines: Vec<&str> = ctx.stdout.lines().map(str::trim).collect();
    let values = context! {
        stdout => ctx.stdout,
        lines => lines,
        exit => ctx.exit,
        case => ctx.case,
    };

    match environment().render_str(text, values) {
        Ok(rendered) => rendered,
        Err(e) if ctx.pending => {
            // No state has arrived yet; the next poll will most likely settle it.
            log::debug!("template `{text}` not ready: {e}");
            String::new()
        }
        Err(e) => {
            // Showing the raw template on the key would only look broken.
            log::warn!("template `{text}` failed: {e}");
            String::new()
        }
    }
}

/// Render an optional piece of text.
pub fn render_opt(text: Option<&str>, ctx: &Context) -> Option<String> {
    text.map(|t| render(t, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(stdout: &str) -> Context {
        Context::from_output(stdout, Some(0), 0)
    }

    #[test]
    fn plain_text_passes_through() {
        assert!(!is_template("Mute"));
        assert_eq!(render("Mute", &ctx("")), "Mute");
    }

    #[test]
    fn state_output_reaches_the_label() {
        assert_eq!(render("Vol {{ stdout }}", &ctx(" 42 ")), "Vol 42");
    }

    #[test]
    fn filters_and_conditions_work() {
        let volume = "Volume: 0.73";
        assert_eq!(
            render(
                "{{ ((stdout | replace('Volume: ', '') | float) * 100) | round | int }}%",
                &ctx(volume)
            ),
            "73%"
        );
        assert_eq!(
            render(
                "mdi:volume-{% if 'MUTED' in stdout %}off{% else %}high{% endif %}",
                &ctx("Volume: 0.73 [MUTED]")
            ),
            "mdi:volume-off"
        );
    }

    #[test]
    fn a_broken_template_renders_empty() {
        // Better an empty label than `{{ … }}` staring back from the key.
        assert_eq!(render("{{ nope( }}", &ctx("")), "");
    }

    #[test]
    fn a_pending_state_is_not_an_error() {
        let pending = Context::default();
        assert!(pending.pending);
        assert_eq!(render("{{ stdout | float }}", &pending), "");
    }
}
