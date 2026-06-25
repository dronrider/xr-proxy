import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type ShareRecord, type CreateShareRequest } from '../api'

export const useSharesStore = defineStore('shares', () => {
  const shares = ref<ShareRecord[]>([])
  const loading = ref(false)

  async function fetchList() {
    loading.value = true
    try {
      shares.value = await api.listShares()
    } finally {
      loading.value = false
    }
  }

  async function create(data: CreateShareRequest): Promise<ShareRecord> {
    const share = await api.createShare(data)
    await fetchList()
    return share
  }

  async function remove(id: string) {
    await api.deleteShare(id)
    await fetchList()
  }

  return { shares, loading, fetchList, create, remove }
})
