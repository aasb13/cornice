//! Display-protocol selection and output ordering, shared by both backends.
//!
//! The two backends have to make the same choices from the same inputs: which
//! protocol to use, and — when `--monitor` is absent — which output is "the
//! main one". Keeping those rules here is what makes the wallpaper and the bar
//! land on the same screen whether they are talking to a Wayland compositor or
//! an X server.

use anyhow::{Context, Result};

/// Which display protocol to draw on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    /// Choose from the environment: Wayland if `WAYLAND_DISPLAY` is set,
    /// otherwise X11 if `DISPLAY` is set.
    Auto,
    Wayland,
    X11,
}

impl Backend {
    /// Resolve `Auto` against the environment. An explicit choice is never
    /// second-guessed — it is the escape hatch for running under XWayland,
    /// where both variables are set and the default would pick Wayland.
    pub fn resolve(self) -> Result<Self> {
        match self {
            Self::Wayland | Self::X11 => Ok(self),
            Self::Auto => detect(
                std::env::var_os("WAYLAND_DISPLAY").as_deref(),
                std::env::var_os("DISPLAY").as_deref(),
            )
            .context("no display found: set WAYLAND_DISPLAY or DISPLAY, or pass --backend"),
        }
    }
}

/// The whole environment decision, kept pure so it can be tested without
/// touching process environment (which is global and racy under parallel
/// tests). Wayland wins when both are set: under XWayland both usually are, and
/// the native protocol is the better choice.
fn detect(wayland: Option<&std::ffi::OsStr>, display: Option<&std::ffi::OsStr>) -> Option<Backend> {
    use std::ffi::OsStr;
    if wayland.is_some_and(|v| v != OsStr::new("")) {
        Some(Backend::Wayland)
    } else if display.is_some_and(|v| v != OsStr::new("")) {
        Some(Backend::X11)
    } else {
        None
    }
}

/// Index of the output to use when `monitor` is `None`.
///
/// Deterministic on purpose. Output lists arrive in advertisement order —
/// whatever order the compositor or X server enumerated them in — which is not
/// specified and can change across a hotplug or a restart. Ordering by geometry
/// (then y, then name) picks the same screen on every run and the same screen
/// `cornice-bar` picks. Leftmost is the conventional "main monitor"; it is a
/// heuristic, not a primary-output query, which neither core Wayland nor core
/// X11 has.
///
/// If the server withholds geometry, every output ties and the name breaks it,
/// which is still deterministic.
pub fn leftmost(outputs: &[(Option<String>, (i32, i32))]) -> Option<usize> {
    outputs
        .iter()
        .enumerate()
        // `is_none` sorts false first, so a named output beats an unnamed one at
        // the same position: it is the one `--monitor` can refer to.
        .min_by_key(|(_, (name, (x, y)))| (*x, *y, name.is_none(), name.clone()))
        .map(|(i, _)| i)
}

/// Every output name, for an error message that says what *was* available.
pub fn names(outputs: &[(Option<String>, (i32, i32))]) -> Vec<&str> {
    outputs
        .iter()
        .map(|(n, _)| n.as_deref().unwrap_or("(unnamed)"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn auto_prefers_wayland_when_both_variables_are_set() {
        let wayland = Some(OsStr::new("wayland-0"));
        let x11 = Some(OsStr::new(":0"));
        assert_eq!(detect(wayland, x11), Some(Backend::Wayland));
        assert_eq!(detect(wayland, None), Some(Backend::Wayland));
    }

    #[test]
    fn auto_falls_back_to_x11_then_fails() {
        assert_eq!(detect(None, Some(OsStr::new(":0"))), Some(Backend::X11));
        assert_eq!(detect(None, None), None);
        // An empty variable is the same as an unset one.
        assert_eq!(detect(Some(OsStr::new("")), Some(OsStr::new(""))), None);
    }

    /// The whole point of the rule: a stable answer, and the leftmost screen, so
    /// the wallpaper lands where the bar does. Reordering the inputs must not
    /// change *which output* is chosen — that is the property advertisement
    /// order fails to give, and comparing names rather than indices is what
    /// proves it.
    #[test]
    fn the_default_output_is_the_leftmost_and_does_not_depend_on_order() {
        let names_and_geometry = [("DP-2", (1920, 0)), ("eDP-1", (0, 0)), ("DP-1", (0, 1080))];
        let chosen = |order: &[usize]| {
            let outputs: Vec<_> = order
                .iter()
                .map(|&i| {
                    let (name, at) = names_and_geometry[i];
                    (Some(name.to_string()), at)
                })
                .collect();
            let i = leftmost(&outputs).expect("non-empty");
            names_and_geometry[order[i]].0
        };

        assert_eq!(
            chosen(&[0, 1, 2]),
            "eDP-1",
            "leftmost is the one at x=0,y=0"
        );
        for order in [[2, 1, 0], [1, 2, 0], [2, 0, 1], [1, 0, 2], [0, 2, 1]] {
            assert_eq!(
                chosen(&order),
                "eDP-1",
                "reordering changed the answer: {order:?}"
            );
        }
    }

    /// Column before row, which is what makes this "leftmost" and not "topmost": a
    /// monitor stacked *below* the origin beats one off to the right, because x
    /// is compared first even though its y is much larger.
    #[test]
    fn x_is_compared_before_y() {
        let outputs = vec![
            (Some("below".to_string()), (0, 1080)),
            (Some("right".to_string()), (1920, 0)),
        ];
        assert_eq!(leftmost(&outputs), Some(0));
    }

    /// Two outputs at the same spot can't happen in practice, but the server is
    /// allowed to report every location as (0,0). The name has to break that tie
    /// or the choice falls back to advertisement order again.
    #[test]
    fn a_server_that_withholds_geometry_still_picks_deterministically() {
        let outputs = vec![
            (Some("DP-1".to_string()), (0, 0)),
            (Some("eDP-1".to_string()), (0, 0)),
        ];
        assert_eq!(leftmost(&outputs), Some(0), "names break the 0,0 tie");
        let mut reversed = outputs.clone();
        reversed.reverse();
        assert_eq!(leftmost(&reversed), Some(1));
    }

    /// An unnamed output still has to be usable — it's just beaten by a named one
    /// at the same position, since `--monitor` can't refer to it.
    #[test]
    fn an_unnamed_output_is_still_a_candidate() {
        assert_eq!(leftmost(&[]), None);
        assert_eq!(leftmost(&[(None, (500, 0))]), Some(0));
        let named = Some("HDMI-1".to_string());
        let outputs = vec![(None, (500, 0)), (named, (500, 0))];
        assert_eq!(leftmost(&outputs), Some(1));
    }

    /// The error has to say what was available, or the user has no way to find
    /// the right name to pass.
    #[test]
    fn the_error_lists_the_names_that_exist() {
        let outputs = vec![(Some("DP-1".to_string()), (0, 0)), (None, (1920, 0))];
        assert_eq!(names(&outputs), vec!["DP-1", "(unnamed)"]);
    }
}
