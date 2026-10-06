// Stable error codes mirrored from src/error_codes.rs. The Rust test
// `the_extension_mirror_lists_exactly_the_same_codes` fails when the two lists drift.
// `action` tells the browser surface what to offer next: retry, refresh, open_app, open_page,
// settings, update, unsupported, report.
const ERROR_CODES = {
  "SSD-BRG-001": { area: "bridge", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-BRG-002": { area: "bridge", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-BRG-003": { area: "bridge", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-BRG-004": { area: "bridge", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-BRG-005": { area: "bridge", stage: "bridge", severity: "error", action: "open_app", retryable: true },
  "SSD-BRG-006": { area: "bridge", stage: "bridge", severity: "error", action: "update", retryable: false },
  "SSD-BRG-007": { area: "bridge", stage: "bridge", severity: "info", action: "retry", retryable: true },
  "SSD-MED-001": { area: "media", stage: "discovery", severity: "error", action: "refresh", retryable: true },
  "SSD-MED-002": { area: "media", stage: "discovery", severity: "error", action: "refresh", retryable: false },
  "SSD-MED-003": { area: "media", stage: "discovery", severity: "error", action: "unsupported", retryable: false },
  "SSD-MED-004": { area: "media", stage: "discovery", severity: "error", action: "unsupported", retryable: false },
  "SSD-MED-005": { area: "media", stage: "discovery", severity: "error", action: "report", retryable: false },
  "SSD-MED-006": { area: "media", stage: "discovery", severity: "error", action: "retry", retryable: true },
  "SSD-MED-007": { area: "media", stage: "discovery", severity: "error", action: "open_page", retryable: true },
  "SSD-MED-008": { area: "media", stage: "discovery", severity: "error", action: "open_page", retryable: false },
  "SSD-MED-009": { area: "media", stage: "validation", severity: "warn", action: "unsupported", retryable: false },
  "SSD-MED-010": { area: "media", stage: "discovery", severity: "warn", action: "refresh", retryable: true },
  "SSD-MED-011": { area: "media", stage: "discovery", severity: "error", action: "retry", retryable: true },
  "SSD-MED-012": { area: "media", stage: "audio", severity: "error", action: "refresh", retryable: true },
  "SSD-MED-013": { area: "media", stage: "subtitle", severity: "error", action: "retry", retryable: true },
  "SSD-MED-014": { area: "media", stage: "validation", severity: "error", action: "settings", retryable: false },
  "SSD-MED-015": { area: "media", stage: "manifest", severity: "error", action: "refresh", retryable: true },
  "SSD-MED-016": { area: "media", stage: "validation", severity: "error", action: "report", retryable: false },
  "SSD-TOL-001": { area: "tools", stage: "source", severity: "error", action: "settings", retryable: true },
  "SSD-TOL-002": { area: "tools", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-TOL-003": { area: "tools", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-TOL-004": { area: "tools", stage: "source", severity: "error", action: "update", retryable: false },
  "SSD-NET-001": { area: "network", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-NET-002": { area: "network", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-NET-003": { area: "network", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-NET-004": { area: "network", stage: "source", severity: "error", action: "settings", retryable: true },
  "SSD-NET-005": { area: "network", stage: "source", severity: "error", action: "refresh", retryable: false },
  "SSD-NET-006": { area: "network", stage: "source", severity: "warn", action: "retry", retryable: true },
  "SSD-NET-007": { area: "network", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-NET-008": { area: "network", stage: "source", severity: "error", action: "retry", retryable: true },
  "SSD-NET-009": { area: "network", stage: "segment", severity: "error", action: "retry", retryable: true },
  "SSD-NET-010": { area: "network", stage: "source", severity: "warn", action: "settings", retryable: true },
  "SSD-TRF-001": { area: "transfer", stage: "segment", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-002": { area: "transfer", stage: "validation", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-003": { area: "transfer", stage: "validation", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-004": { area: "transfer", stage: "output", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-005": { area: "transfer", stage: "output", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-006": { area: "transfer", stage: "output", severity: "error", action: "settings", retryable: true },
  "SSD-TRF-007": { area: "transfer", stage: "output", severity: "error", action: "settings", retryable: false },
  "SSD-TRF-008": { area: "transfer", stage: "source", severity: "info", action: "retry", retryable: true },
  "SSD-TRF-009": { area: "transfer", stage: "source", severity: "info", action: "retry", retryable: true },
  "SSD-TRF-010": { area: "transfer", stage: "source", severity: "info", action: "retry", retryable: true },
  "SSD-TRF-011": { area: "transfer", stage: "fragment", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-012": { area: "transfer", stage: "validation", severity: "error", action: "retry", retryable: true },
  "SSD-TRF-013": { area: "transfer", stage: "source", severity: "info", action: "retry", retryable: true },
  "SSD-STO-001": { area: "storage", stage: "bridge", severity: "error", action: "retry", retryable: true },
  "SSD-STO-002": { area: "storage", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-STO-003": { area: "storage", stage: "source", severity: "warn", action: "settings", retryable: false },
  "SSD-STO-004": { area: "storage", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-STO-005": { area: "storage", stage: "source", severity: "error", action: "settings", retryable: false },
  "SSD-RCV-001": { area: "recovery", stage: "validation", severity: "error", action: "report", retryable: true },
  "SSD-RCV-002": { area: "recovery", stage: "validation", severity: "error", action: "report", retryable: true },
  "SSD-RCV-003": { area: "recovery", stage: "segment", severity: "warn", action: "retry", retryable: true },
  "SSD-RCV-004": { area: "recovery", stage: "validation", severity: "error", action: "report", retryable: true },
  "SSD-UPD-001": { area: "update", stage: "discovery", severity: "error", action: "retry", retryable: true },
  "SSD-UPD-002": { area: "update", stage: "validation", severity: "error", action: "report", retryable: false },
  "SSD-UPD-003": { area: "update", stage: "validation", severity: "error", action: "report", retryable: false },
  "SSD-UPD-004": { area: "update", stage: "output", severity: "error", action: "report", retryable: true },
  "SSD-UPD-005": { area: "update", stage: "output", severity: "warn", action: "report", retryable: true },
  "SSD-UPD-006": { area: "update", stage: "discovery", severity: "info", action: "retry", retryable: true },
  "SSD-EXT-001": { area: "extension", stage: "bridge", severity: "warn", action: "settings", retryable: false },
  "SSD-EXT-002": { area: "extension", stage: "bridge", severity: "warn", action: "refresh", retryable: true },
  "SSD-EXT-003": { area: "extension", stage: "bridge", severity: "warn", action: "refresh", retryable: true },
  "SSD-EXT-004": { area: "extension", stage: "bridge", severity: "warn", action: "refresh", retryable: true },
  "SSD-EXT-005": { area: "extension", stage: "bridge", severity: "warn", action: "refresh", retryable: true },
  "SSD-EXT-006": { area: "extension", stage: "bridge", severity: "error", action: "report", retryable: false },
  "SSD-EXT-007": { area: "extension", stage: "bridge", severity: "warn", action: "unsupported", retryable: false },
  "SSD-EXT-008": { area: "extension", stage: "bridge", severity: "warn", action: "settings", retryable: false },
  "SSD-EXT-009": { area: "extension", stage: "bridge", severity: "info", action: "retry", retryable: true },
  "SSD-EXT-010": { area: "extension", stage: "bridge", severity: "warn", action: "refresh", retryable: true },
};

function codeAction(code) {
  if (!code) return null;
  return ERROR_CODES[code]?.action || null;
}

function isCode(code) {
  return typeof code === "string" && Object.prototype.hasOwnProperty.call(ERROR_CODES, code);
}

function codes() {
  return Object.keys(ERROR_CODES);
}

// Splits a native message of the form `SSD-XXX-NNN rest of the text`.
function splitCode(message) {
  const text = String(message || "");
  const match = text.match(/^(SSD-[A-Z]{3}-\d{3})\s*(.*)$/);
  return match ? { code: match[1], text: match[2] } : { code: null, text };
}

if (typeof globalThis !== "undefined") {
  globalThis.SSDownloadErrorCodes = { ERROR_CODES, codeAction, isCode, codes, splitCode };
}
