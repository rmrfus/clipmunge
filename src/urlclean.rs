//! Remove query parameters while preserving the URL prefix and fragment.

/// Parameters worth dropping by default, matched case-insensitively. A
/// trailing `*` matches a prefix.
pub const DEFAULT_JUNK: &[&str] = &[
    "utm_*",     // UTM tracking parameters
    "fbclid",    // Facebook
    "gclid",     // Google Ads
    "dclid",     // DoubleClick
    "gbraid",    // Google, app campaigns
    "wbraid",    // Google, web-to-app
    "msclkid",   // Microsoft
    "twclid",    // Twitter/X
    "ttclid",    // TikTok
    "igshid",    // Instagram
    "igsh",      // Instagram, short links
    "si",        // Spotify, YouTube
    "mc_cid",    // Mailchimp campaign
    "mc_eid",    // Mailchimp recipient
    "_openstat", // Russian aggregators
    "yclid",     // Yandex
    "ysclid",    // Yandex
    "spm",       // Alibaba
    "scm",       // Alibaba
    "ref_src",   // Twitter/X embeds
    "ref_url",
];

fn is_junk(key: &str, patterns: &[String]) -> bool {
    let key = key.to_ascii_lowercase();
    patterns.iter().any(|p| {
        let p = p.to_ascii_lowercase();
        match p.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == p,
        }
    })
}

/// Return the cleaned URL and removed keys, or `None` if nothing matched.
/// Keys are matched as encoded bytes; empty query segments are omitted on rewrite.
pub fn strip_params(url: &str, patterns: &[String]) -> Option<(String, Vec<String>)> {
    // Split the fragment first: in `https://app/#/board?si=1`, the `?`
    // belongs to a hash route, not the query.
    let (before_hash, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let (head, query) = before_hash.split_once('?')?;

    let mut kept: Vec<&str> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let key = pair.split_once('=').map_or(pair, |(k, _)| k);
        if is_junk(key, patterns) {
            dropped.push(key.to_string());
        } else {
            kept.push(pair);
        }
    }

    if dropped.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(url.len());
    out.push_str(head);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some((out, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<String> {
        DEFAULT_JUNK.iter().map(|s| s.to_string()).collect()
    }

    fn clean(url: &str) -> Option<String> {
        strip_params(url, &defaults()).map(|(u, _)| u)
    }

    #[test]
    fn leaves_a_url_without_a_query_alone() {
        assert_eq!(clean("https://example.com/a"), None);
    }

    #[test]
    fn leaves_a_clean_query_alone() {
        assert_eq!(clean("https://example.com/a?id=7&page=2"), None);
    }

    #[test]
    fn drops_one_and_keeps_the_rest_in_order() {
        assert_eq!(
            clean("https://example.com/a?id=7&utm_source=news&page=2").as_deref(),
            Some("https://example.com/a?id=7&page=2")
        );
    }

    #[test]
    fn removes_the_question_mark_when_nothing_survives() {
        assert_eq!(
            clean("https://example.com/a?utm_source=x&fbclid=y").as_deref(),
            Some("https://example.com/a")
        );
    }

    #[test]
    fn a_trailing_star_matches_a_prefix() {
        assert_eq!(
            clean("https://e.com/?utm_content=a&utm_whatever=b&keep=1").as_deref(),
            Some("https://e.com/?keep=1")
        );
    }

    #[test]
    fn keys_are_matched_case_insensitively() {
        assert_eq!(
            clean("https://e.com/?UTM_Source=a&FBCLID=b&keep=1").as_deref(),
            Some("https://e.com/?keep=1")
        );
    }

    #[test]
    fn the_fragment_survives() {
        assert_eq!(
            clean("https://e.com/doc?utm_source=x&s=2#section-3").as_deref(),
            Some("https://e.com/doc?s=2#section-3")
        );
    }

    #[test]
    fn a_fragment_with_no_query_left_still_survives() {
        assert_eq!(
            clean("https://e.com/doc?fbclid=x#top").as_deref(),
            Some("https://e.com/doc#top")
        );
    }

    #[test]
    fn a_bare_key_with_no_value_is_still_a_parameter() {
        assert_eq!(
            clean("https://e.com/?fbclid&keep=1").as_deref(),
            Some("https://e.com/?keep=1")
        );
        assert_eq!(clean("https://e.com/?keep&other"), None);
    }

    #[test]
    fn junk_inside_a_value_is_not_a_key() {
        // The tracker name appears as data, not as a parameter name.
        assert_eq!(clean("https://e.com/?q=utm_source&r=fbclid"), None);
    }

    #[test]
    fn reports_what_it_dropped() {
        let (_, dropped) =
            strip_params("https://e.com/?si=a&utm_source=b&k=1", &defaults()).unwrap();
        assert_eq!(dropped, vec!["si", "utm_source"]);
    }

    #[test]
    fn cleaning_twice_changes_nothing_the_second_time() {
        // An already cleaned URL should not trigger another rewrite.
        let once = clean("https://e.com/a?utm_source=x&id=1").unwrap();
        assert_eq!(clean(&once), None);
    }

    #[test]
    fn empty_query_is_not_a_change() {
        assert_eq!(clean("https://e.com/a?"), None);
    }

    #[test]
    fn a_question_mark_inside_the_fragment_is_not_a_query() {
        // Hash routing: the `?` belongs to the fragment, so there is no query
        // to clean and the URL must come back untouched.
        assert_eq!(clean("https://app.example.com/#/board?utm_source=x"), None);
    }

    #[test]
    fn a_real_query_is_cleaned_even_with_a_question_mark_in_the_fragment() {
        assert_eq!(
            clean("https://e.com/p?utm_source=x&id=1#/tab?si=2").as_deref(),
            Some("https://e.com/p?id=1#/tab?si=2")
        );
    }

    #[test]
    fn a_custom_list_replaces_the_default() {
        let mine = vec!["id".to_string()];
        assert_eq!(
            strip_params("https://e.com/?id=1&utm_source=x", &mine)
                .map(|(u, _)| u)
                .as_deref(),
            Some("https://e.com/?utm_source=x")
        );
    }
}
