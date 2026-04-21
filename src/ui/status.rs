//! Status display widget.
//!
//! Shows the current pipeline state at the top of the main window:
//!
//! ```text
//! ● CleanMic is active
//! Using: Built-in Microphone → CleanMic
//! ```
//!
//! When the pipeline is inactive:
//!
//! ```text
//! ○ Not running
//! Enable the toggle below to start
//! ```
//!
//! Only compiled when the `gui` feature is enabled.

#[cfg(feature = "gui")]
use gtk4::prelude::*;
#[cfg(feature = "gui")]
use gtk4::{Box as GBox, Label, Orientation};

use crate::ui::UiState;

/// A vertical box containing a headline label and a routing label.
///
/// Call [`StatusWidget::update`] whenever the [`UiState`] changes.
#[cfg(feature = "gui")]
pub struct StatusWidget {
    /// The outer container — add this to the preference group.
    pub container: GBox,
    headline: Label,
    routing: Label,
}

#[cfg(feature = "gui")]
impl StatusWidget {
    /// Construct the status widget with initial state.
    pub fn new(state: &UiState) -> Self {
        let container = GBox::new(Orientation::Vertical, 4);
        container.set_margin_top(12);
        container.set_margin_bottom(8);
        container.set_margin_start(18);
        container.set_margin_end(18);

        let headline = Label::new(None);
        headline.set_halign(gtk4::Align::Start);
        headline.add_css_class("title-4");

        let routing = Label::new(None);
        routing.set_halign(gtk4::Align::Start);
        routing.add_css_class("dim-label");
        routing.add_css_class("caption");

        container.append(&headline);
        container.append(&routing);

        let widget = Self {
            container,
            headline,
            routing,
        };
        widget.update(state);
        widget
    }

    /// Refresh the labels from the current [`UiState`].
    pub fn update(&self, state: &UiState) {
        if state.active {
            self.headline.set_text("CleanMic is active");
            self.headline.remove_css_class("dim-label");

            let device_label = state
                .input_device
                .as_deref()
                .map(|node| {
                    // Try to find a friendly description for the node name.
                    state
                        .available_devices
                        .iter()
                        .find(|d| d.name == node)
                        .map(|d| d.description.as_str())
                        .unwrap_or(node)
                        .to_owned()
                })
                .unwrap_or_else(|| "Default Microphone".to_owned());

            self.routing
                .set_text(&format!("Using: {device_label} \u{2192} CleanMic"));
        } else {
            self.headline.set_text("Not running");
            self.headline.add_css_class("dim-label");
            self.routing.set_text("Enable the toggle below to start");
        }
    }
}
