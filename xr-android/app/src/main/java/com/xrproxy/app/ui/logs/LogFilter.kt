package com.xrproxy.app.ui.logs

/**
 * Pure log filtering for the Log tab (LLD-03 §3.3). Kept out of the
 * ViewModel/Composable so the substring/regex logic is trivially reasoned
 * about (Android has no automated test layer in this project).
 */
data class LogFilterResult(
    val entries: List<String>,
    /** True when regex mode is on but the pattern failed to compile — the UI
     *  shows everything unfiltered and flags the search field. */
    val invalidRegex: Boolean,
)

fun filterLog(all: List<String>, query: String, regex: Boolean): LogFilterResult {
    if (query.isBlank()) return LogFilterResult(all, false)
    if (!regex) {
        return LogFilterResult(all.filter { it.contains(query, ignoreCase = true) }, false)
    }
    val re = runCatching { Regex(query, RegexOption.IGNORE_CASE) }.getOrNull()
        ?: return LogFilterResult(all, true)
    return LogFilterResult(all.filter { re.containsMatchIn(it) }, false)
}
