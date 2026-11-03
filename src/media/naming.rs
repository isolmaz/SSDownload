//! The file name the media picker starts from.

use super::*;

/// The name the picker's field starts from: the name the request carries, or the analysed
/// title with the container's `extension`. A browser handoff (`browser_handoff`) carries the
/// title of the page, and a page whose title is only its site's name (a feed, or a single-page
/// site that has not named the video yet) carries that name for every video it plays, so such
/// a name gives way to the title. A name the user typed is kept whatever it says.
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
        .filter(|name| !(browser_handoff && names_only_the_site(name, request, info)))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "{}.{extension}",
                crate::engine::safe_filename(&info.title, "video")
            )
        })
}

/// Whether `name` is no more than the name of the site the media comes from: the extractor's
/// name for it, or a label of the host (not the top-level one) of the analysed page, the page
/// the browser handed over or the media address. Case, spaces and punctuation do not matter.
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
        Some(url::Host::Domain(host)) => host
            .rsplit('.')
            .skip(1)
            .any(|label| letters_and_digits(label) == name),
        _ => false,
    })
}

fn letters_and_digits(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
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
