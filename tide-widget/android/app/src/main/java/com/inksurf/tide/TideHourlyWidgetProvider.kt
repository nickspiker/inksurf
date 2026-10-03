package com.inksurf.tide

/** The Arabic-numeral edition: HH:MM times with the colon on the bar, a tick every hour. Everything else (sizing, refresh, the shared :X5 alarm) is [TideWidgetProvider]'s. */
class TideHourlyWidgetProvider : TideWidgetProvider() {
    override val hourly = true
}
