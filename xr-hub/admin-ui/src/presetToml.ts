import type { RoutingConfig, RoutingRule } from './api'

/**
 * Печать `[routing]`-блока для превью и для режима «TOML Editor».
 * Имя тематической группы (XR-117) идёт первой строкой правила: по нему
 * владелец пресета понимает, о чём эти двадцать доменов.
 */
export function rulesToToml(defAction: string, rulesList: RoutingRule[]): string {
  let out = `[routing]\ndefault_action = ${tomlString(defAction)}\n`
  for (const rule of rulesList) {
    out += '\n[[routing.rules]]\n'
    if (rule.name) out += `name = ${tomlString(rule.name)}\n`
    out += `action = ${tomlString(rule.action)}\n`
    if (rule.domains.length) {
      out += `domains = [\n${rule.domains.map((d) => `  ${tomlString(d)},`).join('\n')}\n]\n`
    }
    if (rule.ip_ranges.length) {
      out += `ip_ranges = [\n${rule.ip_ranges.map((r) => `  ${tomlString(r)},`).join('\n')}\n]\n`
    }
    if (rule.geoip.length) {
      out += `geoip = [${rule.geoip.map((g) => tomlString(g)).join(', ')}]\n`
    }
  }
  return out
}

/**
 * Строка в TOML-кавычках. Название группы владелец набирает свободным текстом,
 * и кавычка в нём («AI "умный" сервис») без экранирования обрывала бы значение
 * на середине: печать давала битый TOML, а обратный разбор молча возвращал
 * огрызок имени. Экранируем все строки одинаково, включая домены.
 */
function tomlString(value: string): string {
  const escaped = value
    .replace(/\\/g, '\\\\')
    .replace(/"/g, '\\"')
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r')
    .replace(/\t/g, '\\t')
  return `"${escaped}"`
}

function unescapeChar(c: string): string {
  switch (c) {
    case 'n':
      return '\n'
    case 'r':
      return '\r'
    case 't':
      return '\t'
    default:
      return c
  }
}

/** Развернуть экранирование в значении из двойных кавычек. */
function unescapeTomlString(raw: string): string {
  let out = ''
  for (let i = 0; i < raw.length; i++) {
    if (raw[i] === '\\' && i + 1 < raw.length) {
      out += unescapeChar(raw[i + 1])
      i++
    } else {
      out += raw[i]
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
      // стирало бы названия из пресета. Кавычка внутри значения приходит
      // экранированной, поэтому по ней разбор не заканчивается.
      const nameMatch = block.match(/^\s*name\s*=\s*"((?:[^"\\]|\\.)*)"/m)
      const name = nameMatch ? unescapeTomlString(nameMatch[1]) || undefined : undefined

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
    return { error: e instanceof Error ? e.message : `Parse error: ${e}` }
  }
}

/**
 * Элементы массива. Идём по телу посимвольно, а не split'ом по запятым:
 * запятая и решётка внутри кавычек не должны рвать значение, а элемент без
 * кавычек (так пресет мог набрать человек) по-прежнему принимаем. Забытый
 * разделитель между значениями это ошибка разбора, а не потеря домена молчком.
 */
function parseTomlArray(block: string, key: string): string[] {
  const m = block.match(new RegExp(`${key}\\s*=\\s*\\[([^\\]]*?)\\]`, 's'))
  if (!m) return []

  const body = m[1]
  const items: string[] = []
  let bare = ''
  const flushBare = () => {
    const t = bare.trim()
    if (t) items.push(t)
    bare = ''
  }

  let i = 0
  while (i < body.length) {
    const c = body[i]
    if (c === '"' || c === "'") {
      const quote = c
      let value = ''
      i++
      while (i < body.length && body[i] !== quote) {
        // Одинарные кавычки в TOML это literal string, экранирования в них нет.
        if (quote === '"' && body[i] === '\\' && i + 1 < body.length) {
          value += unescapeChar(body[i + 1])
          i += 2
        } else {
          value += body[i]
          i++
        }
      }
      i++
      if (value) items.push(value)
      bare = ''
      // За закрывающей кавычкой законно идут только пробелы, разделитель или
      // комментарий. Всё прочее это забытая запятая (`"a.com" "b.com"`), и
      // молчать про неё нельзя: тихо потерянный домен в пресете значит, что
      // сайт перестал ходить через прокси, а на экране всё как надо.
      while (i < body.length && body[i] !== '\n' && /\s/.test(body[i])) i++
      const tail = body[i]
      if (i < body.length && tail !== ',' && tail !== '\n' && tail !== '#') {
        throw new Error(`${key}: после "${value}" ждали запятую или перенос строки`)
      }
    } else if (c === '#') {
      while (i < body.length && body[i] !== '\n') i++
    } else if (c === ',' || c === '\n') {
      flushBare()
      i++
    } else {
      bare += c
      i++
    }
  }
  flushBare()
  return items
}
