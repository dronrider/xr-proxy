import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type Preset, type PresetSummary, type CreatePresetRequest } from '../api'

export const usePresetsStore = defineStore('presets', () => {
  const summaries = ref<PresetSummary[]>([])
  const loading = ref(false)

  async function fetchList() {
    loading.value = true
    try {
      summaries.value = await api.listPresets()
    } finally {
      loading.value = false
    }
  }

  async function fetchOne(name: string): Promise<Preset> {
    return api.getPreset(name)
  }

  async function create(data: CreatePresetRequest): Promise<Preset> {
    const preset = await api.createPreset(data)
    await fetchList()
    return preset
  }

  async function update(name: string, data: CreatePresetRequest): Promise<Preset> {
    const preset = await api.updatePreset(name, data)
    await fetchList()
    return preset
  }

  async function remove(name: string) {
    await api.deletePreset(name)
    await fetchList()
  }

  return { summaries, loading, fetchList, fetchOne, create, update, remove }
})
