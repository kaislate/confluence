//! Display names: a custom name wins over the device's own, and "show only
//! custom names" hides the device's name wherever a custom one exists
//! (spec: meter bridge §4.4).

use confluence_api::SlotState;

/// (primary, secondary) for something called `device` with an optional
/// `custom` name: the custom name over the device name; just the device
/// name when there is no custom one; just the custom name if `only_custom`.
pub fn display(custom: Option<&str>, device: &str, only_custom: bool) -> (String, Option<String>) {
    match custom.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) if only_custom => (c.to_string(), None),
        Some(c) => (c.to_string(), Some(device.to_string())),
        None => (device.to_string(), None),
    }
}

/// Channel `i` (0-based) of slot `s`, inputs or outputs: its custom name,
/// else the device's name for it, else "In 3" / "Out 3".
pub fn channel(s: &SlotState, input: bool, i: usize) -> String {
    let (labels, names, word) =
        if input { (&s.input_labels, &s.input_names, "In") } else { (&s.output_labels, &s.output_names, "Out") };
    labels
        .get(i)
        .cloned()
        .flatten()
        .or_else(|| names.get(i).filter(|n| !n.trim().is_empty()).cloned())
        .unwrap_or_else(|| format!("{word} {}", i + 1))
}

/// A slot's device as named for display: its custom name, else `fallback`.
pub fn device(s: &SlotState, fallback: &str, only_custom: bool) -> String {
    display(s.label.as_deref(), fallback, only_custom).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use confluence_api::ClockRole;

    fn slot() -> SlotState {
        SlotState {
            id: 1,
            name: "GoXLR".into(),
            device: "asio:GoXLR".into(),
            role: ClockRole::Soft,
            online: true,
            first_input: 0,
            inputs: 3,
            first_output: 0,
            outputs: 0,
            color: None,
            input_names: vec!["Mic".into(), String::new()],
            output_names: Vec::new(),
            label: Some("Desk".into()),
            input_labels: vec![Some("Voice".into()), None, None],
            output_labels: Vec::new(),
        }
    }

    #[test]
    fn a_custom_name_wins_and_can_hide_the_device_name() {
        assert_eq!(display(Some("Desk"), "GoXLR", false), ("Desk".into(), Some("GoXLR".into())));
        assert_eq!(display(Some("Desk"), "GoXLR", true), ("Desk".into(), None));
        assert_eq!(display(None, "GoXLR", true), ("GoXLR".into(), None), "nothing else to show");
        assert_eq!(display(Some("  "), "GoXLR", false), ("GoXLR".into(), None), "blank is no name");
    }

    #[test]
    fn channels_take_the_custom_then_the_device_name_then_a_number() {
        let s = slot();
        assert_eq!(channel(&s, true, 0), "Voice");
        assert_eq!(channel(&s, true, 1), "In 2", "an empty device name falls through");
        assert_eq!(channel(&s, true, 2), "In 3");
        assert_eq!(channel(&s, false, 0), "Out 1");
        assert_eq!(device(&s, "GoXLR ASIO Driver", false), "Desk");
    }
}
