import type { RoutingConfig, RoutingRule } from './api'

/**
 * Печать `[routing]`-блока для превью и для режима «TOML Editor».
 * Имя тематической группы (XR-117) идёт первой строкой правила: по нему
 * владелец пресета понимает, о чём эти двадцать доменов.
 */
export function rulesToToml(defAction: string, rulesList: RoutingRule[]): string {
  let out = `[routing]\ndefault_action = "${defAction}"\n`
  for (const rule of rulesList) {
    out += '\n[[routing.rules]]\n'
    if (rule.name) out += `name = "${rule.name}"\n`
    out += `action = "${rule.action}"\n`
    if (rule.domains.length) {
      out += `domains = [\n${rule.domains.map((d) => `  "${d}",`).join('\n')}\n]\n`
    }
    if (rule.ip_ranges.length) {
      out += `ip_ranges = [\n${rule.ip_ranges.map((r) => `  "${r}",`).join('\n')}\n]\n`
    }
    if (rule.geoip.length) {
      out += `geoip = [${rule.geoip.map((g) => `"${g}"`).join(', ')}]\n`
    }
  }
  return out
}

/**
 * Разбор того же формата обратно. Парсер минимальный, под routing-блок:
 * полноценный TOML тут не нужен, а зависимость в SPA стоит дороже.
 * Ошибка разбора отдаётся строкой, вызывающий показывает её на экране.
 */
export function parseToml(text: string): { config: RoutingConfig } | { error: string } {
  try {
    const daMatch = text.match(/default_action\s*=\s*"(\w+)"/)
    const defAction = daMatch ? daMatch[1] : 'direct'

    const blocks = text.split(/\[\[routing\.rules\]\]/).slice(1)
    const parsed: RoutingRule[] = []

    for (const block of blocks) {
      const actionMatch = block.match(/action\s*=\s*"(\w+)"/)
      const action = actionMatch ? actionMatch[1] : 'proxy'

      // Имя группы читаем обратно, иначе переключение Visual <-> TOML
      // стирало бы названия из пресета.
      const nameMatch = block.match(/^\s*name\s*=\s*"([^"]*)"/m)
      const name = nameMatch?.[1] || undefined

      parsed.push({
        name,
        action,
        domains: parseTomlArray(block, 'domains'),
        ip_ranges: parseTomlArray(block, 'ip_ranges'),
        geoip: parseTomlArray(block, 'geoip'),
      })
    }

    return { config: { default_action: defAction, rules: parsed } }
  } catch (e) {
    return { error: `Parse error: ${e}` }
  }
}

function parseTomlArray(block: string, key: string): string[] {
  const re = new RegExp(`${key}\\s*=\\s*\\[([^\\]]*?)\\]`, 's')
  const m = block.match(re)
  if (!m) return []
  return m[1]
    .split(/,|\n/)
    .map((s) => s.replace(/#.*$/, '').trim().replace(/^["']|["']$/g, ''))
    .filter(Boolean)
}
