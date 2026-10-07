//! Device names as the window shows them (the Devices screen itself is in
//! `devices_screen`).

use confluence_api::DeviceKind;

pub fn kind_title(kind: DeviceKind) -> &'static str {
    match kind {
        DeviceKind::Asio => "ASIO",
        DeviceKind::WasapiRender => "Windows output",
        DeviceKind::WasapiCapture => "Windows input",
        DeviceKind::AppCapture => "App capture",
        DeviceKind::Vasio => "VASIO",
        DeviceKind::Vaio => "VAIO",
        DeviceKind::NetSend => "Network send",
        DeviceKind::NetReceive => "Network receive",
    }
}

/// The listed name an `AddDevice` name belongs to (a VASIO spec is dropped).
pub fn base_name(kind: DeviceKind, name: &str) -> String {
    match kind {
        DeviceKind::Vasio => name.split(':').next().unwrap_or(name).to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_have_readable_titles() {
        assert_eq!(kind_title(DeviceKind::WasapiRender), "Windows output");
        assert_eq!(kind_title(DeviceKind::Vasio), "VASIO");
    }

    #[test]
    fn a_vasio_spec_is_not_part_of_the_listed_name() {
        assert_eq!(base_name(DeviceKind::Vasio, "3:8x2"), "3");
        assert_eq!(base_name(DeviceKind::Asio, "MOTU: Gen 5"), "MOTU: Gen 5");
    }
}
