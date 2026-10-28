// Welcome page, opened once when the extension is installed on a computer without the desktop
// app: it says the extension needs the app and links to the app's download page. Its texts
// follow the same stored language as the popup.
"use strict";
const { t } = SSDownloadI18n;

function applyStaticTexts() {
  document.documentElement.lang = SSDownloadI18n.language();
  document.documentElement.dir = SSDownloadI18n.direction();
  for (const node of document.querySelectorAll("[data-i18n]")) node.textContent = t(node.getAttribute("data-i18n"));
}

SSDownloadI18n.load().then(() => {
  SSDownloadI18n.watch(applyStaticTexts);
  applyStaticTexts();
});
