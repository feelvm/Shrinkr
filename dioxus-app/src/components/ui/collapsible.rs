use dioxus::prelude::*;
use icons::ChevronDown;

/// A disclosure for long explainer text. The trigger is a small muted
/// row (title + chevron) that reads as UI chrome rather than a callout;
/// the prose only appears once it's expanded, chevron flipping over.
#[component]
pub fn Collapsible(
    #[props(into, optional)] class: Option<String>,
    #[props(into)] title: String,
    children: Element,
) -> Element {
    let mut open = use_signal(|| false);

    let chevron_class = if open() {
        "size-3.5 shrink-0 transition-transform rotate-180"
    } else {
        "size-3.5 shrink-0 transition-transform"
    };

    rsx! {
        div {
            "data-name": "Collapsible",
            "data-state": if open() { "open" } else { "closed" },
            class: class.as_deref().unwrap_or(""),
            button {
                r#type: "button",
                class: "flex w-full cursor-pointer select-none items-center gap-1 text-left text-xs font-medium text-muted-foreground transition-colors hover:text-foreground",
                "aria-expanded": "{open()}",
                onclick: move |_| open.toggle(),
                span { "{title}" }
                ChevronDown { class: "{chevron_class}" }
            }
            if open() {
                div { class: "mt-1.5 text-xs leading-relaxed text-muted-foreground", {children} }
            }
        }
    }
}
