import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, type Invite, type CreateInviteRequest } from '../api'

export const useInvitesStore = defineStore('invites', () => {
  const invites = ref<Invite[]>([])
  const loading = ref(false)

  async function fetchList() {
    loading.value = true
    try {
      invites.value = await api.listInvites()
    } finally {
      loading.value = false
    }
  }

  async function create(data: CreateInviteRequest): Promise<Invite> {
    const invite = await api.createInvite(data)
    await fetchList()
    return invite
  }

  async function revoke(token: string) {
    await api.revokeInvite(token)
    await fetchList()
  }

  return { invites, loading, fetchList, create, revoke }
})
