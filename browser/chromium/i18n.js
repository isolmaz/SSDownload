"use strict";

// Every user-visible text of the extension lives here, in English and Turkish.
//
// The interface language is the DESKTOP APP's, not the browser's: the app reports it in its
// capabilities reply (`language`: "tr" | "en"), the service worker stores it in
// chrome.storage.local (`uiLanguage`) and every other context (content scripts, popup)
// reads it from there and follows storage.onChanged. Until the app has said anything (not
// installed, not running, an older build) the browser's own UI language decides: Turkish
// browser -> Turkish, everything else -> English. `_locales/*/messages.json` only carries
// the manifest/store fields because chrome.i18n cannot switch language at run time.
//
// Placeholders are `{0}`, `{1}`, ... filled from the arguments of `t(key, ...args)`; both
// languages of a key use the same set. Load order: first script of every context (the
// service worker's importScripts list, the content_scripts list, popup.html).
(function installI18n(root) {
  const STORAGE_KEY = "uiLanguage";
  const LANGUAGES = ["en", "tr"];

  const en = {
    // ---- toolbar popup, badge and action title
    stateOn: "ON",
    stateOff: "OFF",
    actionTitleOn: "SSDownload: on — click for settings",
    actionTitleOff: "SSDownload: off — click for settings",
    actionTitleOneCompleted: "SSDownload: 1 download completed",
    actionTitleCompleted: "SSDownload: {0} downloads completed",
    popupDetection: "Detection",
    popupHideOnThisSite: "Hide the button on this site",
    popupHideOnHost: "Hide the button on {0}",
    popupMediaHeading: "Media on this page",
    popupNoMedia: "No selectable media found on this page. Play the video and open this again.",
    popupOpenApp: "Open the app",
    popupDownloadApp: "Download the app",
    popupClose: "Close",
    popupKindAudio: "audio",
    popupKindVideo: "video",
    popupOpeningPicker: "Opening the desktop picker…",
    popupNoReply: "The extension background did not respond.",
    popupNoActiveTab: "No active tab was found.",
    popupListRefreshed: "The source list was refreshed. Reopen the popup.",
    popupAppOpenFailed: "SSDownload could not be opened.",
    popupUnknownCommand: "Unknown command",
    popupInvalidSender: "Invalid sender",
    popupInvalidContentSender: "Invalid content sender",

    // ---- in-page tile and page status
    tileVideo: "Download this video",
    tileAudio: "Download this audio",
    tilePage: "Download page with SSDownload",
    tileOptions: "More options",
    tileHideVideo: "Hide on this video",
    tileHideAudio: "Hide on this audio",
    tileHidePage: "Hide on this page",
    tileHideSite: "Hide on this site",
    tileHideSiteHint: "Turn it back on from the SSDownload icon in the browser toolbar.",
    extensionConnectionFailed: "Could not connect to the extension",
    pageUiUpdateFailed: "SSDownload could not update its page interface",
    videoTitleFallback: "Video",

    // ---- context menus
    menuDownloadLink: "Download link with SSDownload",
    menuDownloadVideo: "Download video with SSDownload",
    menuDownloadPage: "Download page with SSDownload",
    menuDownloadAudio: "Download audio with SSDownload",
    menuOpenPicker: "Open the download picker…",
    menuChooseSource: "Choose source ({0})…",
    menuBrowserTransfer: "Browser transfer (refresh source)…",
    menuRelayRestart: "{0} · restart",
    menuSessionGrant: "Allow session cookies for this site",
    menuSessionRevoke: "Remove site session permission",
    menuOpenApp: "Open SSDownload",
    candidateFallbackTitle: "Source",
    candidateFlagPreview: "preview",
    candidateFlagSuspectedAd: "suspected ad",
    candidateFlagAmbiguous: "ambiguous",
    candidateListRefreshed: "The source list was refreshed. Open the menu again.",
    relayListRefreshed: "The source refresh list was refreshed. Open the menu again.",
    sessionGranted: "Session cookies for {0} can be used for this download.",
    sessionRevoked: "Site session permission for {0} was removed.",

    // ---- shared status and refusals (service worker)
    capabilitiesNotVerified: "Desktop capabilities have not been verified yet.",
    capabilityProtocolMismatch: "The desktop app's capability protocol is incompatible. Update the SSDownload desktop app.",
    extensionOff: "SSDownload is off. Click the toolbar icon to turn it on.",
    videoModeOff: "The video usage mode is disabled.",
    audioModeOff: "The audio usage mode is disabled.",
    usageModeOffInDesktop: "This usage mode is disabled in the desktop settings.",
    pageDownloadOff: "Page-level download is disabled in the desktop settings.",
    pickerOpenFailed: "The desktop picker could not be opened.",
    operationFailed: "The operation could not be completed.",
    alreadyQueued: "This download is already in the queue.",
    sessionLoadFailed: "Temporary discovery memory could not be loaded; rescan the page.",
    sessionSaveFailed: "Temporary discovery memory could not be saved; the list may be lost when the background worker stops.",
    logInspectNoReply: "inspection got no reply",
    noSourceHere: "No downloadable source was found here.",
    noSourceOnPage: "No downloadable source was found on this page. Play the video and try again.",
    multipleSourcesOnPage: "This page has more than one source. Right-click and pick the source to download from the Choose source menu.",
    playerGone: "This player is no longer on the page. Play the video and try again.",
    playerAmbiguous: "Several streams belong to this player. Right-click and pick the source to download from the Choose source menu.",
    playerAdOrPreview: "This player may be an ad or a preview; it can only be downloaded by right-clicking and choosing it from the Choose source menu.",
    playerNoSource: "No downloadable source was found for this player. Play the video and try again.",
    pageNotTracked: "This page is no longer being tracked. Reload the page and try again.",
    pageNoVerifiedSource: "No source belonging to a player could be verified on this page. Play the video and try again.",
    pageNotDownloadable: "This page could not be verified as a downloadable source.",

    // ---- discovered source labels and notes
    kindHls: "HLS stream",
    kindDash: "DASH stream",
    kindVideo: "Video",
    kindAudio: "Audio",
    kindFile: "File",
    kindWebrtc: "WebRTC live stream",
    kindBlob: "Browser blob/MSE source",
    noteWebrtc: "WebRTC cannot be downloaded directly.",
    noteBlob: "A blob address cannot be downloaded directly; analyze the page.",
    drmCueLicense: "The page reached a license/DRM endpoint. DRM-protected content is not supported.",
    drmCueEme: "The page used Encrypted Media Extensions. SSDownload does not support DRM-protected streams.",
    embedTitleFallback: "Embedded player",
    playerTitleFallback: "Video {0}",
    noteBlockedPlayerPage: "The player page returned HTTP {0}.",
    noteBlockedPlayerPageUnverified: "The player page returned HTTP {0}; the source on this page could not be verified as the page's own video.",
    noteAmbiguousStreams: "Several streams may belong to this player; use one only by explicit choice.",
    noteSuspectedAd: "It shows independent signs of being an ad; use it only by explicit choice.",
    notePageCandidate: "The player's stream request was not captured; the download will resolve the page address in the app. If that fails, play the video and press Refresh.",
    noteEmbed: "The embedded player's address will be resolved in the app and downloaded.",

    // ---- native connection
    nativeForbidden: "SSDownload's native connection does not allow this extension (the extension ID is not permitted). Install the extension version provided by SSDownload (Browser setup), then restart the browser.",
    nativeAppMissing: "The SSDownload app was not found on this computer. The extension works with the free SSDownload app for Windows: install it with \"Download the app\". If it is installed, open Tools → Browser setup in the app and restart the browser.",
    nativeNotRunning: "SSDownload is not running. Start it with \"Open the app\" in the toolbar popup.",
    nativeAccessDenied: "Access to SSDownload's native connection was denied. Run the app and the browser as the same Windows user.",
    nativeConnectionError: "SSDownload connection error: {0}",
    nativeInvalidReply: "The local app sent an invalid reply",

    // ---- site session (cookies)
    cookieStoreUnverifiable: "This Chrome version cannot verify the current tab's cookie store.",
    cookieStoreAmbiguous: "The current tab's cookie store could not be verified as exactly one.",
    cookieTargetLimit: "The cookie target limit for the selected source chain was exceeded.",
    cookieLimit: "The cookie limit for the selected source chain was exceeded.",
    sessionGrantNeedsPage: "A valid page address is required to grant session access.",
    sessionRevokeNeedsPage: "A valid page address is required to remove session access.",
    cookiePermissionDenied: "Cookie permission was not granted; the site session cannot be used.",
    requestContextExpired: "The source's request context has expired. Play the video again and retry.",
    sessionPermissionQueryFailed: "The site session permission could not be queried. Restart the browser and grant the permission again in this tab.",
    sessionPermissionWrongStore: "The site session permission was not granted for this tab's cookie store. Grant it again in this tab.",
    partitionedScopeUnverifiable: "This Chrome version cannot verify the partitioned session scope. Update the browser or remove the site session permission.",
    partitionScopeUntransferable: "The session scope of this Chrome partition cannot be handed over safely.",

    // ---- browser transfer: service-worker side
    relayTargetNotFound: "The source tab and document for the transfer were not found. Open the page to download in this tab and try again.",
    relayUnavailable: "Browser transfer is not available in this extension version.",
    relayUnknownNativeError: "Unknown native connection error",
    relayNativeForbidden: "SSDownload's native connection does not allow this extension (the extension ID is not permitted).",
    relayNativeMissing: "The SSDownload app was not found on this computer.",
    relayNativeDisconnected: "SSDownload's persistent native connection was lost: {0}",
    relayPausedNativeDisconnect: "The native browser connection dropped; the transfer was explicitly paused: {0}",
    relayStopUnverified: "The browser fetch could not be confirmed stopped; connection permits are kept to be safe: {0}",
    relayUnexpectedNativeReply: "Unexpected Native Messaging reply",
    relayNativeHostClosed: "The Native Messaging host closed",
    relayMessageTooLarge: "The native transfer message exceeds the 1 MiB limit",
    relayQueueFull: "The browser transfer queue reached the 8 MiB limit",
    relayRefused: "The browser transfer was refused",
    relayReplyIncompatible: "The desktop's browser transfer reply is incompatible",
    relayRestartedStopping: "The browser connection was restarted; the active fetch is being explicitly stopped.",
    relayResumeRecordTooLarge: "The browser transfer resume record is too large",
    relaySourceFrameChanged: "The source frame or document is no longer the same",
    relayResumedJobMismatch: "The restarted job does not match the source frame/document",
    relaySourceRejected: "The source document did not accept the transfer",
    relaySourceUnreachable: "The source document could not be reached: {0}",
    relayAbortUnconfirmed: "The source fetch did not confirm that it stopped",
    relayPaused: "Browser transfer paused",
    relayRestarted: "The browser connection was restarted",
    relayTargetInvalid: "The browser transfer target is invalid",
    relayTabClosed: "The source tab is no longer open",
    relayTooManyJobs: "At most eight browser transfers can be held at the same time",
    relayCapabilitiesUnreadable: "The desktop capabilities could not be read",
    relayJobNotAuthorized: "This job was not authorized with the desktop's Refresh source address action",
    relayReplacedByNewRequest: "A new explicit transfer request stopped the previous browser run",
    relayGrantMissing: "The desktop transfer authorization or the source request is missing",
    relayAuthorizedJobMismatch: "The authorized job does not match the selected source frame/document",
    relaySenderInvalid: "The browser transfer sender or authorization is invalid",
    relayStreamIdInvalid: "The stream ID is invalid",
    relayChunkInvalid: "The browser transfer chunk is invalid",
    relaySealInvalid: "The stream sealing information is invalid",
    relayRequestIdInvalid: "The browser connection ID is invalid",
    relayStopping: "The browser transfer is stopping",
    relayPermitLimit: "The browser connection permit limit was exceeded",
    relayPageTransferFailed: "The source page could not complete the transfer",
    relayUnknownInternalCommand: "Unknown internal browser transfer command",
    relayUiOnly: "Browser transfer can only be started from the extension interface",
    relayUserCancelled: "Cancelled by the user",
    relayUserPaused: "The user paused the browser transfer",
    relayUnknownCommand: "Unknown browser transfer command",
    relayTabClosedPaused: "The source tab was closed; the transfer was explicitly paused",
    relayTabNavigatedPaused: "The source tab moved to another document; the transfer was explicitly paused",
    relayTabClosedPermitsFailed: "The browser tab closed but the connection permits could not be ended: {0}",
    relayDocumentClosedPermitsFailed: "The document closed but the desktop permits could not be ended: {0}",

    // ---- browser transfer: page side
    relayStopped: "Browser transfer stopped",
    relayNewAuthorization: "A new transfer authorization was received",
    relayStartInvalid: "The browser transfer start is invalid",
    relayWorkerNoReply: "The extension transfer worker did not respond",
    relayRequestIdFailed: "A browser connection ID could not be generated",
    relayPermitReplyInvalid: "The desktop's connection permit reply is invalid",
    relayConcurrencyLimit: "Browser transfer stopped at the global or per-server concurrency limit",
    relayFetchHttpStatus: "The new request made in the page context returned HTTP {0} ({1})",
    relayRangeUnverified: "The source did not confirm Range resumption; the transfer stopped and the existing data was kept",
    relayEtagChanged: "The source ETag changed during resumption; an explicit restart is required",
    relayNoBodyStream: "The page fetch response provided no streaming body",
    relayCheckpointTooLarge: "The browser resume checkpoint exceeded the 4 KiB limit",
    relayAckMismatch: "The desktop's persistent ACK offset/sequence does not match",
    relayStreamNotReported: "The desktop did not report the opened stream",
    relayResumeRecordMismatch: "The manifest or SHA resume record does not match; an explicit restart is required",
    relayCursorMismatch: "The stream resume cursor does not match the ACK",
    relayMidSegmentNoEtag: "No strong ETag for mid-segment resumption; the existing data is kept and an explicit restart is required",
    relayFileStreamNotOpened: "The desktop did not open the file stream",
    relayFileCursorMismatch: "The file SHA/resume cursor does not match the ACK",
    relayDirectNoEtag: "No strong ETag for resuming the direct file; an explicit restart is required",
    relayDirectWithTracks: "A direct combined file cannot be transferred as a graph of separately selected tracks",
    relayDirectNowManifest: "The earlier direct stream is now a manifest; an explicit restart is required",
    roleVideo: "video",
    roleAudio: "audio",
    roleSubtitle: "subtitle",
    manifestBadAddress: "The manifest contains an address that is not HTTP/HTTPS or is too long",
    manifestTooLarge: "The manifest exceeds the 2 MiB safety limit",
    hlsNoVideoVariant: "The HLS master playlist has no video variant",
    hlsVariantIdAmbiguous: "The selected HLS video format ID does not match exactly one variant",
    hlsResolutionMissing: "The requested HLS resolution is not in the manifest; no other quality was substituted",
    hlsAutoSubtitleUnverifiable: "The automatic subtitle ID cannot be verified in the HLS manifest; no manual subtitle was substituted",
    hlsRenditionAmbiguous: "The selected HLS {0} rendition does not match exactly one URI",
    hlsLiveUnsupported: "Live or unfinished HLS playlists are not supported by browser transfer",
    hlsDiscontinuity: "HLS discontinuities and live transitions cannot be merged safely",
    hlsEncrypted: "Encrypted/DRM HLS streams are not supported by browser transfer",
    hlsByterange: "HLS BYTERANGE segments are not supported yet; no incomplete film was produced",
    hlsAdMarkers: "An HLS playlist with ad or transition markers is not transferred as a single film",
    hlsSegmentDurationInvalid: "An HLS segment duration is invalid",
    hlsInitUnsupported: "The URI/BYTERANGE form of the HLS initialization segment is not supported",
    hlsSegmentLimit: "The HLS segment count limit was exceeded",
    hlsVodIncomplete: "The HLS VOD segment/duration record is incomplete",
    hlsSignatureInvalid: "The HLS signature is invalid",
    hlsExternalSubtitleMuxed: "An external subtitle cannot be added safely to a combined HLS/file stream",
    hlsAudioGroupMissing: "The selected HLS audio group was not found",
    hlsSeparateSubtitleMuxedAudio: "A separate subtitle cannot be added losslessly to an HLS variant with combined audio",
    dashTemplateMediaMissing: "The DASH SegmentTemplate media template is missing",
    dashTimelineUnsupported: "Open-ended or malformed DASH SegmentTimeline is not supported",
    dashSegmentLimit: "The DASH segment count limit was exceeded",
    dashDurationTemplateIncomplete: "A duration-based DASH SegmentTemplate cannot be completed",
    dashMediaRange: "DASH mediaRange is not supported yet",
    dashSegmentListEmpty: "The DASH SegmentList is empty",
    dashSegmentBaseUnsupported: "DASH SegmentBase or an undefined segment graph is not supported",
    dashFormatIdMissing: "The selected DASH {0} format ID was not found",
    dashResolutionMissing: "The requested DASH resolution was not found; the quality was not changed",
    dashXmlInvalid: "The DASH XML manifest is invalid",
    dashMpdRootMissing: "The DASH MPD root was not found",
    dashLiveUnsupported: "Live/dynamic DASH transfer is not supported",
    dashEncrypted: "Encrypted/DRM DASH streams are not supported",
    dashMultiPeriod: "Multi-period DASH transitions/ads are not transferred as a single film",
    dashAudioLanguageAmbiguous: "The selected DASH audio language does not match exactly one track",
    dashAutoSubtitleUnverifiable: "The automatic subtitle ID cannot be verified in the DASH manifest; no manual subtitle was substituted",
    dashSubtitleLanguageAmbiguous: "The selected DASH subtitle language does not match exactly one track",
    dashNoStream: "No selected stream was found in the DASH manifest",
    sha256ResumeRecordInvalid: "Invalid SHA-256 resume record",
    sha256ResumeTailInvalid: "The SHA-256 resume tail is invalid",
    sha256LengthExceeded: "The SHA-256 length exceeded the supported limit"
  };

  const tr = {
    // ---- toolbar popup, badge and action title
    stateOn: "AÇIK",
    stateOff: "KAPALI",
    actionTitleOn: "SSDownload: açık — ayarlar için tıklayın",
    actionTitleOff: "SSDownload: kapalı — ayarlar için tıklayın",
    actionTitleOneCompleted: "SSDownload: 1 indirme tamamlandı",
    actionTitleCompleted: "SSDownload: {0} indirme tamamlandı",
    popupDetection: "Algılama",
    popupHideOnThisSite: "Bu sitede düğmeyi gösterme",
    popupHideOnHost: "{0} için düğmeyi gösterme",
    popupMediaHeading: "Bu sayfadaki medya",
    popupNoMedia: "Bu sayfada seçilebilir medya bulunamadı. Videoyu oynatıp tekrar açın.",
    popupOpenApp: "Uygulamayı aç",
    popupDownloadApp: "Uygulamayı indir",
    popupClose: "Kapat",
    popupKindAudio: "ses",
    popupKindVideo: "video",
    popupOpeningPicker: "Masaüstü seçicisi açılıyor…",
    popupNoReply: "Eklenti arka planı yanıt vermedi.",
    popupNoActiveTab: "Etkin sekme bulunamadı.",
    popupListRefreshed: "Kaynak listesi yenilendi. Pencereyi yeniden açın.",
    popupAppOpenFailed: "SSDownload açılamadı.",
    popupUnknownCommand: "Bilinmeyen komut",
    popupInvalidSender: "Geçersiz gönderici",
    popupInvalidContentSender: "Geçersiz içerik göndericisi",

    // ---- in-page tile and page status
    tileVideo: "Bu videoyu indir",
    tileAudio: "Bu sesi indir",
    tilePage: "Sayfayı SSDownload ile indir",
    tileOptions: "Diğer seçenekler",
    tileHideVideo: "Bu videoda gizle",
    tileHideAudio: "Bu seste gizle",
    tileHidePage: "Bu sayfada gizle",
    tileHideSite: "Bu sitede gizle",
    tileHideSiteHint: "Tarayıcı araç çubuğundaki SSDownload simgesinden yeniden açabilirsiniz.",
    extensionConnectionFailed: "Eklenti bağlantısı kurulamadı",
    pageUiUpdateFailed: "SSDownload sayfa arayüzü güncellenemedi",
    videoTitleFallback: "Video",

    // ---- context menus
    menuDownloadLink: "Bağlantıyı SSDownload ile indir",
    menuDownloadVideo: "Videoyu SSDownload ile indir",
    menuDownloadPage: "Sayfayı SSDownload ile indir",
    menuDownloadAudio: "Sesi SSDownload ile indir",
    menuOpenPicker: "İndirme penceresini aç…",
    menuChooseSource: "Kaynak seç ({0})…",
    menuBrowserTransfer: "Tarayıcı aktarımı (kaynak yenileme)…",
    menuRelayRestart: "{0} · yeniden başlat",
    menuSessionGrant: "Bu site için oturum çerezlerine izin ver",
    menuSessionRevoke: "Site oturum iznini kaldır",
    menuOpenApp: "SSDownload uygulamasını aç",
    candidateFallbackTitle: "Kaynak",
    candidateFlagPreview: "önizleme",
    candidateFlagSuspectedAd: "reklam şüphesi",
    candidateFlagAmbiguous: "belirsiz",
    candidateListRefreshed: "Kaynak listesi yenilendi. Menüyü yeniden açın.",
    relayListRefreshed: "Kaynak yenileme listesi yenilendi. Menüyü yeniden açın.",
    sessionGranted: "{0} için oturum çerezleri bu indirmede kullanılabilir.",
    sessionRevoked: "{0} için site oturum izni kaldırıldı.",

    // ---- shared status and refusals (service worker)
    capabilitiesNotVerified: "Masaüstü özellikleri henüz doğrulanmadı.",
    capabilityProtocolMismatch: "Masaüstü uygulamasının özellik protokolü uyumsuz. SSDownload masaüstü uygulamasını güncelleyin.",
    extensionOff: "SSDownload kapalı. Açmak için araç çubuğundaki simgeye tıklayın.",
    videoModeOff: "Video kullanım modu kapalı.",
    audioModeOff: "Ses kullanım modu kapalı.",
    usageModeOffInDesktop: "Bu kullanım modu masaüstü ayarlarında kapalı.",
    pageDownloadOff: "Sayfa düzeyi indirme masaüstü ayarlarında kapalı.",
    pickerOpenFailed: "Masaüstü indirme penceresi açılamadı.",
    operationFailed: "İşlem tamamlanamadı.",
    alreadyQueued: "Bu indirme zaten kuyruğa eklendi.",
    sessionLoadFailed: "Geçici keşif belleği yüklenemedi; sayfayı yeniden tarayın.",
    sessionSaveFailed: "Geçici keşif belleği kaydedilemedi; arka plan işçisi kapanınca liste kaybolabilir.",
    logInspectNoReply: "inceleme yanıtsız kaldı",
    noSourceHere: "Burada indirilebilir bir kaynak bulunamadı.",
    noSourceOnPage: "Bu sayfada indirilebilir kaynak bulunamadı. Videoyu oynatıp yeniden deneyin.",
    multipleSourcesOnPage: "Bu sayfada birden fazla kaynak var. Sağ tıklayıp Kaynak seç menüsünden indirilecek kaynağı seçin.",
    playerGone: "Bu oynatıcı artık sayfada bulunamadı. Videoyu oynatıp yeniden deneyin.",
    playerAmbiguous: "Bu oynatıcıyla birden fazla yayın ilişkili. Sağ tıklayıp Kaynak seç menüsünden indirilecek kaynağı seçin.",
    playerAdOrPreview: "Bu oynatıcı bir reklam veya önizleme olabilir; yalnız sağ tıklayıp Kaynak seç menüsünden indirilebilir.",
    playerNoSource: "Bu oynatıcı için indirilebilir kaynak bulunamadı. Videoyu oynatıp yeniden deneyin.",
    pageNotTracked: "Bu sayfa artık izlenmiyor. Sayfayı yenileyip yeniden deneyin.",
    pageNoVerifiedSource: "Bu sayfada oynatıcıya ait bir kaynak doğrulanamadı. Videoyu oynatıp yeniden deneyin.",
    pageNotDownloadable: "Bu sayfa indirilebilir bir kaynak olarak doğrulanamadı.",

    // ---- discovered source labels and notes
    kindHls: "HLS yayını",
    kindDash: "DASH yayını",
    kindVideo: "Video",
    kindAudio: "Ses",
    kindFile: "Dosya",
    kindWebrtc: "WebRTC canlı akışı",
    kindBlob: "Tarayıcı blob/MSE kaynağı",
    noteWebrtc: "WebRTC doğrudan indirilemez.",
    noteBlob: "Blob adresi doğrudan indirilemez; sayfayı analiz edin.",
    drmCueLicense: "Sayfa bir lisans/DRM uç noktasına erişti. DRM korumalı içerik desteklenmez.",
    drmCueEme: "Sayfa Encrypted Media Extensions kullandı. DRM korumalı akış SSDownload tarafından desteklenmez.",
    embedTitleFallback: "Gömülü oynatıcı",
    playerTitleFallback: "Video {0}",
    noteBlockedPlayerPage: "Oynatıcı sayfası HTTP {0} döndürdü.",
    noteBlockedPlayerPageUnverified: "Oynatıcı sayfası HTTP {0} döndürdü; bu sayfadaki kaynak bölüm videosu olarak doğrulanamadı.",
    noteAmbiguousStreams: "Bu oynatıcıyla birden fazla yayın ilişkili olabilir; yalnız açık seçimle kullanın.",
    noteSuspectedAd: "Bağımsız reklam belirtileri taşıyor; yalnız açık seçimle kullanın.",
    notePageCandidate: "Oynatıcının akış isteği yakalanamadı; indirme, sayfa adresi uygulamada çözümlenerek yapılacak. Sonuç alınamazsa videoyu oynatıp Yenile'ye basın.",
    noteEmbed: "Gömülü oynatıcı adresi uygulamada çözümlenerek indirilecek.",

    // ---- native connection
    nativeForbidden: "SSDownload yerel bağlantısı bu eklentiye izin vermiyor (eklenti kimliği izinli değil). SSDownload'ın sunduğu eklenti sürümünü kurun (Tarayıcı Kurulumu); ardından tarayıcıyı yeniden başlatın.",
    nativeAppMissing: "SSDownload uygulaması bu bilgisayarda bulunamadı. Eklenti, Windows için ücretsiz SSDownload uygulamasıyla çalışır: \"Uygulamayı indir\" ile kurun. Kuruluysa uygulamada Araçlar → Tarayıcı kurulumu'nu açın ve tarayıcıyı yeniden başlatın.",
    nativeNotRunning: "SSDownload çalışmıyor. Araç çubuğu penceresindeki \"Uygulamayı aç\" ile başlatın.",
    nativeAccessDenied: "SSDownload yerel bağlantısına erişim reddedildi. Uygulamayı ve tarayıcıyı aynı Windows kullanıcısıyla çalıştırın.",
    nativeConnectionError: "SSDownload bağlantı hatası: {0}",
    nativeInvalidReply: "Yerel uygulama geçersiz yanıt verdi",

    // ---- site session (cookies)
    cookieStoreUnverifiable: "Bu Chrome sürümünde geçerli sekmenin çerez deposu doğrulanamıyor.",
    cookieStoreAmbiguous: "Geçerli sekmenin çerez deposu tek olarak doğrulanamadı.",
    cookieTargetLimit: "Seçili kaynak zinciri için çerez hedefi sınırı aşıldı.",
    cookieLimit: "Seçili kaynak zincirinin çerez sınırı aşıldı.",
    sessionGrantNeedsPage: "Oturum izni için geçerli bir sayfa adresi gerekli.",
    sessionRevokeNeedsPage: "Oturum izni kaldırmak için geçerli bir sayfa adresi gerekli.",
    cookiePermissionDenied: "Çerez izni verilmedi; site oturumu kullanılamaz.",
    requestContextExpired: "Kaynağın istek bağlamı eskidi. Videoyu yeniden oynatıp yeniden deneyin.",
    sessionPermissionQueryFailed: "Site oturum izni sorgulanamadı. Tarayıcıyı yeniden başlatıp izni bu sekmede yeniden verin.",
    sessionPermissionWrongStore: "Site oturum izni bu sekmenin çerez deposu için verilmiş değil. İzni bu sekmede yeniden verin.",
    partitionedScopeUnverifiable: "Bu Chrome sürümünde bölümlenmiş oturum kapsamı doğrulanamıyor. Tarayıcıyı güncelleyin veya site oturumu iznini kaldırın.",
    partitionScopeUntransferable: "Bu Chrome bölümünün oturum kapsamı güvenle aktarılamıyor.",

    // ---- browser transfer: service-worker side
    relayTargetNotFound: "Aktarım için kaynak sekmesi ve belgesi bulunamadı. İndirilecek sayfayı bu sekmede açıp yeniden deneyin.",
    relayUnavailable: "Tarayıcı aktarımı bu eklenti sürümünde kullanılamıyor.",
    relayUnknownNativeError: "Bilinmeyen yerel bağlantı hatası",
    relayNativeForbidden: "SSDownload yerel bağlantısı bu eklentiye izin vermiyor (eklenti kimliği izinli değil).",
    relayNativeMissing: "SSDownload uygulaması bu bilgisayarda bulunamadı.",
    relayNativeDisconnected: "SSDownload kalıcı yerel bağlantısı kesildi: {0}",
    relayPausedNativeDisconnect: "Yerel tarayıcı bağlantısı koptu; aktarım açıkça duraklatıldı: {0}",
    relayStopUnverified: "Tarayıcı fetch durması doğrulanamadı; bağlantı izinleri güvenli olarak tutuluyor: {0}",
    relayUnexpectedNativeReply: "Beklenmeyen Native Messaging yanıtı",
    relayNativeHostClosed: "Native Messaging host kapandı",
    relayMessageTooLarge: "Yerel aktarım iletisi 1 MiB sınırını aşıyor",
    relayQueueFull: "Tarayıcı aktarım kuyruğu 8 MiB sınırına ulaştı",
    relayRefused: "Tarayıcı aktarımı reddedildi",
    relayReplyIncompatible: "Masaüstü tarayıcı aktarım yanıtı uyumsuz",
    relayRestartedStopping: "Tarayıcı bağlantısı yeniden başlatıldı; etkin fetch işlemi açıkça durduruluyor.",
    relayResumeRecordTooLarge: "Tarayıcı aktarım sürdürme kaydı çok büyük",
    relaySourceFrameChanged: "Kaynak frame veya belge artık aynı değil",
    relayResumedJobMismatch: "Yeniden başlayan iş kaynak frame/belge ile eşleşmiyor",
    relaySourceRejected: "Kaynak belge aktarımı kabul etmedi",
    relaySourceUnreachable: "Kaynak belgeye ulaşılamadı: {0}",
    relayAbortUnconfirmed: "Kaynak fetch işlemi durmayı doğrulamadı",
    relayPaused: "Tarayıcı aktarımı duraklatıldı",
    relayRestarted: "Tarayıcı bağlantısı yeniden başlatıldı",
    relayTargetInvalid: "Tarayıcı aktarım hedefi geçersiz",
    relayTabClosed: "Kaynak sekmesi artık açık değil",
    relayTooManyJobs: "Aynı anda en fazla sekiz tarayıcı aktarımı tutulabilir",
    relayCapabilitiesUnreadable: "Masaüstü özellikleri okunamadı",
    relayJobNotAuthorized: "Bu iş masaüstündeki Kaynak adresini yenile eylemiyle yetkilendirilmedi",
    relayReplacedByNewRequest: "Yeni açık aktarım isteği önceki tarayıcı çalışmasını durdurdu",
    relayGrantMissing: "Masaüstü aktarım yetkisi veya kaynak isteği eksik",
    relayAuthorizedJobMismatch: "Yetkili iş seçilen kaynak frame/belge ile eşleşmiyor",
    relaySenderInvalid: "Tarayıcı aktarım göndereni veya yetkisi geçersiz",
    relayStreamIdInvalid: "Akış kimliği geçersiz",
    relayChunkInvalid: "Tarayıcı aktarım parçası geçersiz",
    relaySealInvalid: "Akış mühürleme bilgisi geçersiz",
    relayRequestIdInvalid: "Tarayıcı bağlantı kimliği geçersiz",
    relayStopping: "Tarayıcı aktarımı durduruluyor",
    relayPermitLimit: "Tarayıcı bağlantı izni sayısı sınırı aşıldı",
    relayPageTransferFailed: "Kaynak sayfa aktarımı tamamlayamadı",
    relayUnknownInternalCommand: "Bilinmeyen tarayıcı aktarım iç komutu",
    relayUiOnly: "Tarayıcı aktarımı yalnız eklenti arayüzünden başlatılabilir",
    relayUserCancelled: "Kullanıcı iptal etti",
    relayUserPaused: "Kullanıcı tarayıcı aktarımını duraklattı",
    relayUnknownCommand: "Bilinmeyen tarayıcı aktarım komutu",
    relayTabClosedPaused: "Kaynak sekmesi kapandı; aktarım açıkça duraklatıldı",
    relayTabNavigatedPaused: "Kaynak sekmesi başka bir belgeye geçti; aktarım açıkça duraklatıldı",
    relayTabClosedPermitsFailed: "Tarayıcı sekmesi kapandı ancak bağlantı izinleri sonlandırılamadı: {0}",
    relayDocumentClosedPermitsFailed: "Belge kapandı ancak masaüstü izinleri sonlandırılamadı: {0}",

    // ---- browser transfer: page side
    relayStopped: "Tarayıcı aktarımı durduruldu",
    relayNewAuthorization: "Yeni aktarım yetkisi alındı",
    relayStartInvalid: "Tarayıcı aktarım başlangıcı geçersiz",
    relayWorkerNoReply: "Eklenti aktarım işçisi yanıt vermedi",
    relayRequestIdFailed: "Tarayıcı bağlantı kimliği üretilemedi",
    relayPermitReplyInvalid: "Masaüstü bağlantı izni yanıtı geçersiz",
    relayConcurrencyLimit: "Tarayıcı aktarımı küresel veya sunucu eşzamanlılık sınırında durduruldu",
    relayFetchHttpStatus: "Sayfa bağlamındaki yeni istek HTTP {0} döndürdü ({1})",
    relayRangeUnverified: "Kaynak Range sürdürmesini doğrulamadı; eski veri korunarak aktarım durduruldu",
    relayEtagChanged: "Kaynak ETag sürdürme sırasında değişti; açık yeniden başlatma gerekli",
    relayNoBodyStream: "Sayfa fetch yanıtı akış gövdesi sağlamadı",
    relayCheckpointTooLarge: "Tarayıcı sürdürme kontrol kaydı 4 KiB sınırını aştı",
    relayAckMismatch: "Masaüstü kalıcı ACK ofseti/sırası eşleşmiyor",
    relayStreamNotReported: "Masaüstü açılan akışı bildirmedi",
    relayResumeRecordMismatch: "Manifest veya SHA sürdürme kaydı eşleşmiyor; açık yeniden başlatma gerekli",
    relayCursorMismatch: "Akış sürdürme imleci ACK ile eşleşmiyor",
    relayMidSegmentNoEtag: "Parça ortası sürdürme için güçlü ETag yok; eski veri korunarak açık yeniden başlatma gerekli",
    relayFileStreamNotOpened: "Masaüstü dosya akışını açmadı",
    relayFileCursorMismatch: "Dosya SHA/sürdürme imleci ACK ile eşleşmiyor",
    relayDirectNoEtag: "Doğrudan dosya sürdürmesi için güçlü ETag yok; açık yeniden başlatma gerekli",
    relayDirectWithTracks: "Doğrudan birleşik dosya ayrı seçili track grafiği olarak aktarılamaz",
    relayDirectNowManifest: "Önceki doğrudan akış şimdi manifest oldu; açık yeniden başlatma gerekli",
    roleVideo: "görüntü",
    roleAudio: "ses",
    roleSubtitle: "altyazı",
    manifestBadAddress: "Manifest HTTP/HTTPS dışında veya çok uzun bir adres içeriyor",
    manifestTooLarge: "Manifest 2 MiB güvenlik sınırını aşıyor",
    hlsNoVideoVariant: "HLS ana listesinde görüntü varyantı yok",
    hlsVariantIdAmbiguous: "Seçilen HLS görüntü format kimliği tek bir varyantla eşleşmiyor",
    hlsResolutionMissing: "İstenen HLS çözünürlüğü manifestte yok; başka kaliteye geçilmedi",
    hlsAutoSubtitleUnverifiable: "HLS manifestinde otomatik altyazı kimliği doğrulanamıyor; manuel altyazıya geçilmedi",
    hlsRenditionAmbiguous: "Seçilen HLS {0} parçası tek bir URI ile eşleşmiyor",
    hlsLiveUnsupported: "Canlı veya tamamlanmamış HLS listesi tarayıcı aktarımında desteklenmez",
    hlsDiscontinuity: "HLS discontinuity/live transition güvenli biçimde birleştirilemez",
    hlsEncrypted: "Şifreli/DRM HLS akışı tarayıcı aktarımında desteklenmez",
    hlsByterange: "HLS BYTERANGE parçaları henüz desteklenmez; eksik film üretilmedi",
    hlsAdMarkers: "Reklam/geçiş işaretli HLS listesi tek film olarak aktarılmaz",
    hlsSegmentDurationInvalid: "HLS parça süresi geçersiz",
    hlsInitUnsupported: "HLS başlatma parçası URI/BYTERANGE biçimi desteklenmiyor",
    hlsSegmentLimit: "HLS parça sayısı sınırı aşıldı",
    hlsVodIncomplete: "HLS VOD parça/süre kaydı eksik",
    hlsSignatureInvalid: "HLS imzası geçersiz",
    hlsExternalSubtitleMuxed: "Birleşik HLS/file akışına harici altyazı güvenle eklenemiyor",
    hlsAudioGroupMissing: "Seçilen HLS ses grubu bulunamadı",
    hlsSeparateSubtitleMuxedAudio: "Birleşik sesli HLS varyantına ayrı altyazı kayıpsız eklenemiyor",
    dashTemplateMediaMissing: "DASH SegmentTemplate media şablonu eksik",
    dashTimelineUnsupported: "DASH açık uçlu/bozuk SegmentTimeline desteklenmez",
    dashSegmentLimit: "DASH parça sayısı sınırı aşıldı",
    dashDurationTemplateIncomplete: "DASH süre tabanlı SegmentTemplate tamamlanamıyor",
    dashMediaRange: "DASH mediaRange henüz desteklenmez",
    dashSegmentListEmpty: "DASH SegmentList boş",
    dashSegmentBaseUnsupported: "DASH SegmentBase veya tanımsız parça grafiği desteklenmez",
    dashFormatIdMissing: "Seçilen DASH {0} format kimliği bulunamadı",
    dashResolutionMissing: "İstenen DASH çözünürlüğü bulunamadı; kalite değiştirilmedi",
    dashXmlInvalid: "DASH XML manifesti geçersiz",
    dashMpdRootMissing: "DASH MPD kökü bulunamadı",
    dashLiveUnsupported: "Canlı/dynamic DASH aktarımı desteklenmez",
    dashEncrypted: "Şifreli/DRM DASH akışı desteklenmez",
    dashMultiPeriod: "Çok dönemli DASH geçişleri/reklamları tek film olarak aktarılmaz",
    dashAudioLanguageAmbiguous: "Seçilen DASH ses dili tek bir parçayla eşleşmiyor",
    dashAutoSubtitleUnverifiable: "DASH manifestinde otomatik altyazı kimliği doğrulanamıyor; manuel altyazıya geçilmedi",
    dashSubtitleLanguageAmbiguous: "Seçilen DASH altyazı dili tek bir parçayla eşleşmiyor",
    dashNoStream: "DASH manifestinde seçili akış bulunamadı",
    sha256ResumeRecordInvalid: "Geçersiz SHA-256 sürdürme kaydı",
    sha256ResumeTailInvalid: "SHA-256 sürdürme kuyruğu geçersiz",
    sha256LengthExceeded: "SHA-256 uzunluğu desteklenen sınırı aştı"
  };

  const MESSAGES = Object.freeze({ en: Object.freeze(en), tr: Object.freeze(tr) });

  // The language the desktop app reported (or the stored copy of it); null until known.
  let appLanguage = null;

  function extensionApi() { return root.browser || root.chrome; }

  // Turkish browser -> Turkish, anything else -> English.
  function browserLanguage() {
    try {
      const ui = extensionApi()?.i18n?.getUILanguage?.();
      return /^tr(?:[-_]|$)/i.test(String(ui || "")) ? "tr" : "en";
    } catch (_) { return "en"; }
  }

  function language() { return appLanguage || browserLanguage(); }

  // Returns true when the app-reported language changed (so a caller knows to persist it).
  // Anything that is not a supported language forgets the app's choice.
  function setLanguage(value) {
    const next = LANGUAGES.includes(value) ? value : null;
    if (next === appLanguage) return false;
    appLanguage = next;
    return true;
  }

  function t(key, ...args) {
    const text = MESSAGES[language()][key] ?? MESSAGES.en[key] ?? key;
    return text.replace(/\{(\d+)\}/g, (_, index) => {
      const value = args[Number(index)];
      return value === undefined || value === null ? "" : String(value);
    });
  }

  // Reads the stored app language; storage that is absent or failing leaves the browser's.
  async function load() {
    try {
      const saved = await extensionApi()?.storage?.local?.get?.({ [STORAGE_KEY]: null });
      setLanguage(saved?.[STORAGE_KEY]);
    } catch (_) { /* keep the current language */ }
    return language();
  }

  // Follows the stored language live; `listener` runs with the effective language after each change.
  function watch(listener) {
    try {
      extensionApi()?.storage?.onChanged?.addListener?.((changes, area) => {
        if (area !== "local" || !changes?.[STORAGE_KEY]) return;
        setLanguage(changes[STORAGE_KEY].newValue);
        listener(language());
      });
    } catch (_) { /* no storage events in this context */ }
  }

  root.SSDownloadI18n = { t, language, setLanguage, load, watch, STORAGE_KEY, LANGUAGES, MESSAGES };
})(globalThis);
