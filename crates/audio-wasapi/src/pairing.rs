//! Which capture endpoint belongs with a render endpoint (ADR 0016, slice d): the user's pairing
//! when it names an endpoint that is present, else the endpoint of the same physical device. Two
//! endpoints are the same device when they report the same `IMMDevice` container id
//! (`PKEY_Device_ContainerId`). Pure data, so the rule is tested without COM.

/// What the rule needs to know of an endpoint.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) container: Option<&'a str>,
}

/// The container id as compared: lowercase without braces; the all-zero id (virtual endpoints with
/// no physical device) is no id at all.
pub(crate) fn normalize_container(text: &str) -> Option<String> {
    let id = text.trim().trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase();
    (!id.is_empty() && id.chars().any(|c| c != '0' && c != '-')).then_some(id)
}

/// The index in `inputs` that pairs with `render`: the one `pairing` names (by endpoint id or by
/// name) when it is among them, else the first with the render endpoint's container id. A pairing
/// that is not present (the interface is unplugged) does not block the automatic rule.
pub(crate) fn pair(render: Candidate<'_>, inputs: &[Candidate<'_>], pairing: Option<&str>) -> Option<usize> {
    let user = pairing.filter(|p| !p.is_empty()).and_then(|wanted| inputs.iter().position(|i| i.id == wanted || i.name == wanted));
    user.or_else(|| {
        let container = render.container.and_then(normalize_container)?;
        inputs.iter().position(|i| i.container.and_then(normalize_container).as_deref() == Some(container.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCARLETT: &str = "{8C7ED206-3F8A-4827-B3AB-AE9E1FAEFC6C}";

    fn endpoint<'a>(id: &'a str, name: &'a str, container: Option<&'a str>) -> Candidate<'a> {
        Candidate { id, name, container }
    }

    #[test]
    fn endpoints_with_one_container_pair_whatever_their_order_or_brace_and_case() {
        let render = endpoint("r1", "Speakers (Scarlett)", Some(SCARLETT));
        let inputs = [
            endpoint("i1", "Microphone (Realtek)", Some("{11111111-2222-3333-4444-555555555555}")),
            endpoint("i2", "Line In (Scarlett)", Some("8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c")),
        ];
        assert_eq!(pair(render, &inputs, None), Some(1));
    }

    #[test]
    fn different_containers_and_missing_or_zero_container_ids_do_not_pair() {
        let inputs = [endpoint("i1", "Mic", Some("{11111111-2222-3333-4444-555555555555}")), endpoint("i2", "Virtual", None)];
        assert_eq!(pair(endpoint("r", "Out", Some(SCARLETT)), &inputs, None), None);
        assert_eq!(pair(endpoint("r", "Out", None), &inputs, None), None);
        let zero = "{00000000-0000-0000-0000-000000000000}";
        assert_eq!(pair(endpoint("r", "Virtual out", Some(zero)), &[endpoint("i", "Virtual in", Some(zero))], None), None);
    }

    #[test]
    fn the_user_pairing_wins_over_the_container_and_matches_by_id_or_name() {
        let render = endpoint("r1", "Speakers", Some(SCARLETT));
        let inputs = [endpoint("i1", "Mic A", Some(SCARLETT)), endpoint("i2", "Mic B", None)];
        assert_eq!(pair(render, &inputs, Some("i2")), Some(1));
        assert_eq!(pair(render, &inputs, Some("Mic B")), Some(1));
        assert_eq!(pair(render, &inputs, Some("")), Some(0), "an empty pairing is no pairing");
    }

    #[test]
    fn a_pairing_to_an_absent_endpoint_falls_back_to_the_container() {
        let render = endpoint("r1", "Speakers", Some(SCARLETT));
        let inputs = [endpoint("i1", "Mic A", Some(SCARLETT))];
        assert_eq!(pair(render, &inputs, Some("gone")), Some(0));
        assert_eq!(pair(endpoint("r2", "Other", None), &inputs, Some("gone")), None);
    }

    #[test]
    fn user_pairing_pairs_devices_that_share_no_container() {
        let render = endpoint("r1", "Speakers (USB DAC)", Some("{aaaaaaaa-0000-0000-0000-000000000001}"));
        let inputs = [endpoint("i1", "Mic (Webcam)", Some("{bbbbbbbb-0000-0000-0000-000000000002}"))];
        assert_eq!(pair(render, &inputs, None), None);
        assert_eq!(pair(render, &inputs, Some("i1")), Some(0));
    }
}
