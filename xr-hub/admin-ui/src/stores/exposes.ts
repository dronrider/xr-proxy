import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type ExposeRecord } from '../api'

export const useExposesStore = defineStore('exposes', () => {
  const exposes = ref<ExposeRecord[]>([])
  const loading = ref(false)

  async function fetchList() {
    loading.value = true
    try {
      exposes.value = await api.listExposes()
    } finally {
      loading.value = false
    }
  }

  async function remove(name: string) {
    await api.deleteExpose(name)
    await fetchList()
  }

  return { exposes, loading, fetchList, remove }
})
