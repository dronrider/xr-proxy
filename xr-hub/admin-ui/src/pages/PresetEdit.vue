<script setup lang="ts">
import { ref, onMounted, computed, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { usePresetsStore } from '../stores/presets'
import RulesEditor from '../components/RulesEditor.vue'
import type { RoutingRule, RoutingConfig } from '../api'

const route = useRoute()
const router = useRouter()
const store = usePresetsStore()

const isNew = computed(() => route.name === 'PresetCreate')
const presetName = computed(() => (route.params.name as string) || '')

const name = ref('')
const description = ref('')
const defaultAction = ref('direct')
const rules = ref<RoutingRule[]>([])
const version = ref(0)
const updatedAt = ref('')
const saving = ref(false)
const error = ref('')
const toast = ref('')

function showToast(msg: string) {
  toast.value = msg
  setTimeout(() => (toast.value = ''), 3000)
}

// Visual editor vs raw TOML editor
const mode = ref<'visual' | 'toml'>('visual')
const rawToml = ref('')
const tomlError = ref('')

onMounted(async () => {
  if (!isNew.value && presetName.value) {
    try {
      const preset = await store.fetchOne(presetName.value)
      name.value = preset.name
      description.value = preset.description
      defaultAction.value = preset.rules.default_action
      rules.value = preset.rules.rules
      version.value = preset.version
      updatedAt.value = preset.updated_at
    } catch (e) {
      error.value = `Failed to load preset: ${e}`
    }
  }
})

// Sync: when switching to TOML mode, serialize current rules
watch(mode, (newMode) => {
  if (newMode === 'toml') {
    rawToml.value = rulesToToml(defaultAction.value, rules.value)
    tomlError.value = ''
  } else {
    // Switching back to visual — parse TOML into rules
    const parsed = parseToml(rawToml.value)
    if (parsed) {
      defaultAction.value = parsed.default_action
      rules.value = parsed.rules
      tomlError.value = ''
    }
  }
})

async function save() {
  error.value = ''

  // If in TOML mode, parse first
  if (mode.value === 'toml') {
    const parsed = parseToml(rawToml.value)
    if (!parsed) {
      error.value = tomlError.value || 'Failed to parse TOML'
      return
    }
    defaultAction.value = parsed.default_action
    rules.value = parsed.rules
  }

  saving.value = true
  try {
    const config: RoutingConfig = {
      default_action: defaultAction.value,
      rules: rules.value,
    }
    if (isNew.value) {
      await store.create({ name: name.value, description: description.value, rules: config })
      router.push(`/presets/${name.value}`)
    } else {
      const updated = await store.update(presetName.value, {
        name: presetName.value,
        description: description.value,
        rules: config,
      })
      version.value = updated.version
      updatedAt.value = updated.updated_at
      showToast(`Preset saved (v${updated.version})`)
    }
  } catch (e) {
    error.value = `${e}`
  } finally {
    saving.value = false
  }
}

// ── TOML serializer ──

function rulesToToml(defAction: string, rulesList: RoutingRule[]): string {
  let out = `[routing]\ndefault_action = "${defAction}"\n`
  for (const rule of rulesList) {
    out += `\n[[routing.rules]]\naction = "${rule.action}"\n`
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

// ── TOML parser (minimal, for routing config format) ──

function parseToml(text: string): RoutingConfig | null {
  try {
    // Extract default_action
    const daMatch = text.match(/default_action\s*=\s*"(\w+)"/)
    const defAction = daMatch ? daMatch[1] : 'direct'

    // Split by [[routing.rules]]
    const blocks = text.split(/\[\[routing\.rules\]\]/).slice(1)
    const parsed: RoutingRule[] = []

    for (const block of blocks) {
      const actionMatch = block.match(/action\s*=\s*"(\w+)"/)
      const action = actionMatch ? actionMatch[1] : 'proxy'

      const domains = parseTomlArray(block, 'domains')
      const ip_ranges = parseTomlArray(block, 'ip_ranges')
      const geoip = parseTomlArray(block, 'geoip')

      parsed.push({ action, domains, ip_ranges, geoip })
    }

    tomlError.value = ''
    return { default_action: defAction, rules: parsed }
  } catch (e) {
    tomlError.value = `Parse error: ${e}`
    return null
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

const tomlPreview = computed(() => rulesToToml(defaultAction.value, rules.value))
</script>

<template>
  <div>
    <div class="page-header">
      <h2>{{ isNew ? 'New Preset' : `Preset: ${presetName}` }}</h2>
      <span v-if="!isNew" class="meta">v{{ version }} | {{ updatedAt }}</span>
    </div>

    <p v-if="error" class="error">{{ error }}</p>

    <div class="field" v-if="isNew">
      <label>Name (slug)</label>
      <input v-model="name" placeholder="e.g. russia" pattern="[a-z0-9_-]+" />
    </div>

    <div class="field">
      <label>Description</label>
      <input v-model="description" placeholder="Optional description" />
    </div>

    <div class="mode-switcher">
      <button
        :class="{ active: mode === 'visual' }"
        @click="mode = 'visual'"
      >Visual Editor</button>
      <button
        :class="{ active: mode === 'toml' }"
        @click="mode = 'toml'"
      >TOML Editor</button>
    </div>

    <!-- Visual mode -->
    <div v-if="mode === 'visual'" class="edit-layout">
      <div class="edit-form">
        <div class="field">
          <label>Default Action</label>
          <select v-model="defaultAction">
            <option value="direct">direct</option>
            <option value="proxy">proxy</option>
          </select>
        </div>

        <RulesEditor v-model="rules" />
      </div>

      <div class="preview">
        <h3>TOML Preview</h3>
        <pre>{{ tomlPreview }}</pre>
      </div>
    </div>

    <!-- TOML mode -->
    <div v-if="mode === 'toml'" class="toml-editor">
      <p class="toml-hint">
        Вставьте или отредактируйте правила в формате TOML.
        Формат аналогичен <code>routing-russia.toml</code>.
      </p>
      <p v-if="tomlError" class="error">{{ tomlError }}</p>
      <textarea
        v-model="rawToml"
        class="toml-textarea"
        spellcheck="false"
      ></textarea>
    </div>

    <button class="btn-primary" @click="save" :disabled="saving">
      {{ saving ? 'Saving...' : 'Save' }}
    </button>

    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>

<style scoped>
.page-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 1.5rem;
}

.meta { color: var(--text-muted); font-size: 0.875rem; }
.error { color: var(--danger); margin-bottom: 1rem; }

.mode-switcher {
  display: flex;
  gap: 0;
  margin-bottom: 1.5rem;
}

.mode-switcher button {
  padding: 0.5rem 1.5rem;
  border: 1px solid var(--border);
  background: var(--bg);
  color: var(--text);
  cursor: pointer;
  font-size: 0.875rem;
}

.mode-switcher button:first-child { border-radius: 4px 0 0 4px; }
.mode-switcher button:last-child { border-radius: 0 4px 4px 0; border-left: none; }

.mode-switcher button.active {
  background: var(--btn-bg);
  color: var(--btn-text);
  border-color: var(--btn-bg);
}

.edit-layout {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 2rem;
}

@media (max-width: 800px) {
  .edit-layout { grid-template-columns: 1fr; }
}

.field { margin-bottom: 1rem; }
.field label { display: block; margin-bottom: 0.25rem; font-weight: 600; font-size: 0.875rem; color: var(--text); }
.field input, .field select {
  width: 100%;
  padding: 0.5rem;
  border: 1px solid var(--border);
  border-radius: 4px;
  background: var(--bg-input);
  color: var(--text);
}

.btn-primary {
  margin-top: 1rem;
  padding: 0.5rem 2rem;
  background: var(--btn-bg);
  color: var(--btn-text);
  border: none;
  border-radius: 4px;
  cursor: pointer;
}
.btn-primary:disabled { opacity: 0.5; }

.preview { background: var(--bg-preview); border-radius: 8px; padding: 1rem; }
.preview h3 { margin-bottom: 0.5rem; font-size: 0.875rem; color: var(--text-muted); }
.preview pre { font-size: 0.8rem; white-space: pre-wrap; overflow-x: auto; color: var(--text); }

.toml-editor { margin-bottom: 1rem; }
.toml-hint { font-size: 0.85rem; color: var(--text-muted); margin-bottom: 0.75rem; }
.toml-hint code { background: var(--bg-preview); padding: 0.1rem 0.3rem; border-radius: 3px; }

.toml-textarea {
  width: 100%;
  min-height: 500px;
  padding: 1rem;
  font-family: 'SF Mono', 'Fira Code', monospace;
  font-size: 0.85rem;
  line-height: 1.5;
  border: 1px solid var(--border);
  border-radius: 4px;
  background: var(--bg-input);
  color: var(--text);
  resize: vertical;
  tab-size: 2;
}
</style>
