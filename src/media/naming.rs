//! The file name the media picker starts from.

use super::*;

/// What the browser extension calls a video, a player or an embed that has no title, in each
/// language it speaks. It sends one of them, with the player's number after it, as the file
/// name; it names nothing.
const EXTENSION_PLACEHOLDERS: [&str; 12] = [
    "Video",
    "Vídeo",
    "Vidéo",
    "فيديو",
    "الفيديو",
    "Embedded player",
    "Gömülü oynatıcı",
    "مشغّل مضمَّن",
    "Reproductor incrustado",
    "Reprodutor incorporado",
    "Lecteur intégré",
    "Eingebetteter Player",
];

/// The name the picker's field starts from: the name the request carries, or the analysed
/// title with the container's `extension`. A browser handoff (`browser_handoff`) carries the
/// title of the page, and a page whose title is only its site's name (a feed, or a single-page
/// site that has not named the video yet) carries that name for every video it plays, as does
/// a placeholder of the extension for an untitled one, so such a name gives way to the title.
/// A name the user typed is kept whatever it says.
pub(crate) fn picker_file_name(
    request: &AddRequest,
    browser_handoff: bool,
    info: &MediaInfo,
    extension: &str,
) -> String {
    request
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| {
            !(browser_handoff
                && (is_extension_placeholder(name) || names_only_the_site(name, request, info)))
        })
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}.{extension}",
                crate::engine::safe_filename(&info.title, "video")
            )
        })
}

/// Whether `name` is one of the extension's placeholders, with or without the player's number.
fn is_extension_placeholder(name: &str) -> bool {
    let name = letters_and_digits(name);
    let stem = name.trim_end_matches(|character: char| character.is_ascii_digit());
    EXTENSION_PLACEHOLDERS
        .iter()
        .any(|placeholder| letters_and_digits(placeholder) == stem)
}

/// Whether `name` is no more than the name of the site the media comes from: the extractor's
/// name for it, or a label of the host (not the top-level one) of the analysed page, the page
/// the browser handed over or the media address, in its Unicode form as well as its ASCII one.
/// Case, spaces, punctuation and a leading unread count do not matter.
fn names_only_the_site(name: &str, request: &AddRequest, info: &MediaInfo) -> bool {
    let name = letters_and_digits(name);
    if name.is_empty() {
        return false;
    }
    if letters_and_digits(&info.extractor) == name {
        return true;
    }
    [
        Some(info.webpage_url.as_str()),
        request.page_url.as_deref(),
        Some(request.url.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter_map(|address| Url::parse(address).ok())
    .any(|address| match address.host() {
        Some(url::Host::Domain(host)) => {
            let unicode = url::quirks::domain_to_unicode(host);
            host.rsplit('.')
                .skip(1)
                .chain(unicode.rsplit('.').skip(1))
                .any(|label| letters_and_digits(label) == name)
        }
        _ => false,
    })
}

fn letters_and_digits(value: &str) -> String {
    without_unread_count(value)
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

/// A tab title leads with the unread count some sites keep there: `(3) Clips`.
fn without_unread_count(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
        .filter(|(count, rest)| {
            !count.is_empty()
                && count.bytes().all(|byte| byte.is_ascii_digit())
                && rest.starts_with(char::is_whitespace)
        })
        .map_or(value, |(_, rest)| rest.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(title: &str, extractor: &str, webpage_url: &str) -> MediaInfo {
        MediaInfo {
            title: title.into(),
            extractor: extractor.into(),
            webpage_url: webpage_url.into(),
            ..Default::default()
        }
    }

    fn request(name: Option<&str>, url: &str, page_url: Option<&str>) -> AddRequest {
        AddRequest {
            url: url.into(),
            page_url: page_url.map(Into::into),
            filename: name.map(Into::into),
            ..Default::default()
        }
    }

    const MEDIA: &str = "https://cdn.example/master.m3u8";

    #[test]
    fn a_site_name_from_the_browser_gives_way_to_the_media_title_for_the_extractor_name() {
        let info = info("Real Title", "Moviehub", MEDIA);
        let request = request(Some("MovieHub"), MEDIA, None);
        assert_eq!(
            picker_file_name(&request, true, &info, "mkv"),
            "Real Title.mkv"
        );
    }

    #[test]
    fn a_site_name_from_the_browser_gives_way_to_the_media_title_for_a_host_label() {
        let from_page = request(
            Some(" Clips. "),
            MEDIA,
            Some("https://www.clips.example/watch?v=1"),
        );
        let from_media = request(Some("CLIPS"), "https://clips.example/v/master.m3u8", None);
        let from_analysis = request(Some("clips"), MEDIA, None);
        for (request, analysed) in [
            (from_page, info("Real Title", "Generic", MEDIA)),
            (from_media, info("Real Title", "Generic", MEDIA)),
            (
                from_analysis,
                info("Real Title", "Generic", "https://clips.example/watch"),
            ),
        ] {
            assert_eq!(
                picker_file_name(&request, true, &analysed, "mp4"),
                "Real Title.mp4",
                "{request:?}"
            );
        }
    }

    #[test]
    fn a_name_that_says_more_than_the_site_is_kept() {
        let info = info("Real Title", "Moviehub", "https://clips.example/watch");
        let request = request(
            Some(" Real Film - Clips "),
            MEDIA,
            Some("https://clips.example/watch"),
        );
        assert_eq!(
            picker_file_name(&request, true, &info, "mkv"),
            "Real Film - Clips"
        );
    }

    #[test]
    fn neither_the_top_level_label_nor_an_ip_address_is_a_site_name() {
        let on_domain = request(Some("example"), "https://clips.example/v.m3u8", None);
        let on_address = request(Some("127"), "http://127.0.0.1:8080/v.m3u8", None);
        let analysed = info("Real Title", "Generic", "");
        assert_eq!(
            picker_file_name(&on_domain, true, &analysed, "mkv"),
            "example"
        );
        assert_eq!(picker_file_name(&on_address, true, &analysed, "mkv"), "127");
    }

    #[test]
    fn a_name_the_user_typed_is_kept_even_when_it_is_the_site_name() {
        let info = info("Real Title", "Moviehub", "https://clips.example/watch");
        let request = request(Some("Clips"), MEDIA, Some("https://clips.example/watch"));
        assert_eq!(picker_file_name(&request, false, &info, "mkv"), "Clips");
    }

    #[test]
    fn a_placeholder_of_the_extension_gives_way_to_the_media_title_in_every_language() {
        let info = info("Real Title", "Generic", "https://clips.example/watch");
        for name in [
            "Video",
            "Video 3",
            "Vídeo 12",
            "Vidéo",
            "فيديو",
            "الفيديو 4",
            "Embedded player",
            "Gömülü oynatıcı",
            "مشغّل مضمَّن",
            "Reproductor incrustado",
            "Reprodutor incorporado",
            "Lecteur intégré",
            "Eingebetteter Player 2",
            " VIDEO 7 ",
            "(3) Video",
        ] {
            assert_eq!(
                picker_file_name(&request(Some(name), MEDIA, None), true, &info, "mp4"),
                "Real Title.mp4",
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_that_only_starts_with_a_placeholder_word_is_kept() {
        let info = info("Real Title", "Generic", "https://clips.example/watch");
        for name in ["Video Game Night", "Video 2: Finale", "Player One"] {
            assert_eq!(
                picker_file_name(&request(Some(name), MEDIA, None), true, &info, "mp4"),
                name
            );
        }
        assert_eq!(
            picker_file_name(&request(Some("Video 3"), MEDIA, None), false, &info, "mp4"),
            "Video 3",
            "a name the user typed is kept"
        );
    }

    #[test]
    fn the_known_placeholders_cover_the_wording_of_every_extension_language() {
        let catalog = include_str!("../../browser/chromium/i18n.js");
        let placeholders: Vec<String> = catalog
            .lines()
            .map(str::trim)
            .filter_map(|line| {
                [
                    "videoTitleFallback",
                    "playerTitleFallback",
                    "embedTitleFallback",
                ]
                .into_iter()
                .find_map(|key| {
                    line.strip_prefix(key)?
                        .strip_prefix(": \"")?
                        .strip_suffix("\",")
                })
            })
            .map(|text| text.replace("{0}", "3"))
            .collect();
        assert_eq!(placeholders.len(), 21, "{placeholders:?}");
        for placeholder in placeholders {
            assert!(is_extension_placeholder(&placeholder), "{placeholder}");
        }
    }

    #[test]
    fn a_leading_unread_count_does_not_hide_a_site_name() {
        let info = info("Real Title", "Moviehub", "https://clips.example/watch");
        for name in ["(3) Clips", "(12)   MovieHub", " (7) clips "] {
            assert_eq!(
                picker_file_name(
                    &request(Some(name), MEDIA, Some("https://clips.example/watch")),
                    true,
                    &info,
                    "mp4"
                ),
                "Real Title.mp4",
                "{name}"
            );
        }
        for name in ["(3) Real Film", "(1917)", "(3)Clips", "(x) Clips"] {
            assert_eq!(
                picker_file_name(
                    &request(Some(name), MEDIA, Some("https://clips.example/watch")),
                    true,
                    &info,
                    "mp4"
                ),
                name
            );
        }
    }

    #[test]
    fn a_site_name_is_matched_in_the_unicode_form_of_an_international_host() {
        let info = info("Real Title", "Generic", MEDIA);
        let request = request(Some("Bücher"), "https://bücher.example/v/master.m3u8", None);
        assert_eq!(
            picker_file_name(&request, true, &info, "mp4"),
            "Real Title.mp4"
        );
    }

    #[test]
    fn a_site_name_with_a_dotted_capital_i_is_matched() {
        let info = info("Real Title", "Generic", MEDIA);
        let request = request(
            Some("İnstagram"),
            "https://instagram.example/v/1.m3u8",
            None,
        );
        assert_eq!(
            picker_file_name(&request, true, &info, "mp4"),
            "Real Title.mp4"
        );
    }

    #[test]
    fn without_a_name_the_media_title_names_the_file() {
        let info = info("Real Title", "Moviehub", MEDIA);
        for name in [None, Some(""), Some("  ")] {
            assert_eq!(
                picker_file_name(&request(name, MEDIA, None), true, &info, "mkv"),
                "Real Title.mkv"
            );
        }
    }
}
