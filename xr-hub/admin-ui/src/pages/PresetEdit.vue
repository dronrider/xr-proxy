<script setup lang="ts">
import { ref, onMounted, computed } from 'vue'
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

async function save() {
  error.value = ''
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
    }
  } catch (e) {
    error.value = `${e}`
  } finally {
    saving.value = false
  }
}

const tomlPreview = computed(() => {
  let out = `[routing]\ndefault_action = "${defaultAction.value}"\n`
  for (const rule of rules.value) {
    out += `\n[[routing.rules]]\naction = "${rule.action}"\n`
    if (rule.domains.length) {
      out += `domains = [\n${rule.domains.map((d) => `  "${d}"`).join(',\n')}\n]\n`
    }
    if (rule.ip_ranges.length) {
      out += `ip_ranges = [\n${rule.ip_ranges.map((r) => `  "${r}"`).join(',\n')}\n]\n`
    }
    if (rule.geoip.length) {
      out += `geoip = [${rule.geoip.map((g) => `"${g}"`).join(', ')}]\n`
    }
  }
  return out
})
</script>

<template>
  <div>
    <div class="page-header">
      <h2>{{ isNew ? 'New Preset' : `Preset: ${presetName}` }}</h2>
      <span v-if="!isNew" class="meta">v{{ version }} | {{ updatedAt }}</span>
    </div>

    <p v-if="error" class="error">{{ error }}</p>

    <div class="edit-layout">
      <div class="edit-form">
        <div class="field" v-if="isNew">
          <label>Name (slug)</label>
          <input v-model="name" placeholder="e.g. russia" pattern="[a-z0-9_-]+" />
        </div>

        <div class="field">
          <label>Description</label>
          <input v-model="description" placeholder="Optional description" />
        </div>

        <div class="field">
          <label>Default Action</label>
          <select v-model="defaultAction">
            <option value="direct">direct</option>
            <option value="proxy">proxy</option>
          </select>
        </div>

        <RulesEditor v-model="rules" />

        <button class="btn-primary" @click="save" :disabled="saving">
          {{ saving ? 'Saving...' : 'Save' }}
        </button>
      </div>

      <div class="preview">
        <h3>TOML Preview</h3>
        <pre>{{ tomlPreview }}</pre>
      </div>
    </div>
  </div>
</template>

<style scoped>
.page-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 1.5rem;
}

.meta {
  color: #999;
  font-size: 0.875rem;
}

.error {
  color: #d32f2f;
  margin-bottom: 1rem;
}

.edit-layout {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 2rem;
}

@media (max-width: 800px) {
  .edit-layout {
    grid-template-columns: 1fr;
  }
}

.field {
  margin-bottom: 1rem;
}

.field label {
  display: block;
  margin-bottom: 0.25rem;
  font-weight: 600;
  font-size: 0.875rem;
}

.field input,
.field select {
  width: 100%;
  padding: 0.5rem;
  border: 1px solid #ccc;
  border-radius: 4px;
}

.btn-primary {
  margin-top: 1rem;
  padding: 0.5rem 2rem;
  background: #1a1a2e;
  color: #fff;
  border: none;
  border-radius: 4px;
  cursor: pointer;
}

.btn-primary:disabled {
  opacity: 0.5;
}

.preview {
  background: #f9f9f9;
  border-radius: 8px;
  padding: 1rem;
}

.preview h3 {
  margin-bottom: 0.5rem;
  font-size: 0.875rem;
  color: #666;
}

.preview pre {
  font-size: 0.8rem;
  white-space: pre-wrap;
  overflow-x: auto;
}
</style>
