import { describe, expect, it } from 'vitest'
import { parseToml, rulesToToml } from './presetToml'
import type { RoutingRule } from './api'

const messengers: RoutingRule = {
  name: 'Мессенджеры',
  action: 'proxy',
  domains: ['telegram.org', '*.telegram.org'],
  ip_ranges: ['91.108.56.0/22'],
  geoip: [],
}

const unnamed: RoutingRule = {
  action: 'direct',
  domains: ['gosuslugi.ru'],
  ip_ranges: [],
  geoip: [],
}

describe('rulesToToml', () => {
  it('печатает имя группы первой строкой правила', () => {
    expect(rulesToToml('direct', [messengers])).toBe(
      '[routing]\ndefault_action = "direct"\n' +
        '\n[[routing.rules]]\n' +
        'name = "Мессенджеры"\n' +
        'action = "proxy"\n' +
        'domains = [\n  "telegram.org",\n  "*.telegram.org",\n]\n' +
        'ip_ranges = [\n  "91.108.56.0/22",\n]\n',
    )
  })

  it('правило без имени печатается как раньше', () => {
    expect(rulesToToml('direct', [unnamed])).toBe(
      '[routing]\ndefault_action = "direct"\n' +
        '\n[[routing.rules]]\n' +
        'action = "direct"\n' +
        'domains = [\n  "gosuslugi.ru",\n]\n',
    )
  })
})

describe('parseToml', () => {
  /** Переключение Visual <-> TOML гоняет пресет через обе функции, и потеря
   *  имени здесь стёрла бы названия групп в боевом пресете. */
  it('round-trip сохраняет имена групп и правила без имён', () => {
    const result = parseToml(rulesToToml('direct', [messengers, unnamed]))
    expect('config' in result).toBe(true)
    if (!('config' in result)) return
    expect(result.config.default_action).toBe('direct')
    expect(result.config.rules[0]).toEqual(messengers)
    expect(result.config.rules[1].name).toBeUndefined()
    expect(result.config.rules[1].domains).toEqual(['gosuslugi.ru'])
  })

  /** Имя набирается свободным текстом, и кавычка в нём раньше обрывала
   *  значение: round-trip возвращал «AI » вместо «AI "умный" сервис», причём
   *  молча, без ошибки на экране. */
  it('кавычки и слеши в названии группы переживают round-trip', () => {
    const tricky: RoutingRule = {
      name: 'AI "умный" сервис \\ и прочее',
      action: 'proxy',
      domains: ['openai.com'],
      ip_ranges: [],
      geoip: [],
    }
    const toml = rulesToToml('direct', [tricky])
    expect(toml).toContain('name = "AI \\"умный\\" сервис \\\\ и прочее"')

    const result = parseToml(toml)
    expect('config' in result).toBe(true)
    if (!('config' in result)) return
    expect(result.config.rules[0].name).toBe(tricky.name)
    expect(result.config.rules[0].domains).toEqual(['openai.com'])
  })

  it('комментарий в списке доменов не попадает в правила', () => {
    const result = parseToml(
      '[routing]\ndefault_action = "direct"\n\n[[routing.rules]]\nname = "Мессенджеры"\naction = "proxy"\ndomains = [\n  # Telegram\n  "t.me",\n]\n',
    )
    expect('config' in result).toBe(true)
    if (!('config' in result)) return
    expect(result.config.rules[0].domains).toEqual(['t.me'])
  })

  /** Пресет правится руками в режиме «TOML Editor», и забытая запятая раньше
   *  стоила бы домена молчком: сайт перестал бы ходить через прокси, а на
   *  экране всё выглядело бы сохранённым. */
  it('забытая запятая между доменами это ошибка на экране, а не потерянный домен', () => {
    const result = parseToml(
      '[routing]\ndefault_action = "direct"\n\n[[routing.rules]]\naction = "proxy"\ndomains = ["a.com" "b.com"]\n',
    )
    expect('error' in result).toBe(true)
    if (!('error' in result)) return
    expect(result.error).toContain('ждали запятую')
  })

  it('комментарий и пробелы после значения разбору не мешают', () => {
    const result = parseToml(
      '[routing]\ndefault_action = "direct"\n\n[[routing.rules]]\naction = "proxy"\ndomains = [\n  "t.me" ,  # Telegram\n  "discord.gg"  # Discord\n]\n',
    )
    expect('config' in result).toBe(true)
    if (!('config' in result)) return
    expect(result.config.rules[0].domains).toEqual(['t.me', 'discord.gg'])
  })

  it('пресет, набранный руками до появления имён, читается без них', () => {
    const result = parseToml(
      '[routing]\ndefault_action = "direct"\n\n[[routing.rules]]\naction = "proxy"\ndomains = ["youtube.com"]\n',
    )
    expect('config' in result).toBe(true)
    if (!('config' in result)) return
    expect(result.config.rules).toHaveLength(1)
    expect(result.config.rules[0].name).toBeUndefined()
    expect(result.config.rules[0].domains).toEqual(['youtube.com'])
  })
})
