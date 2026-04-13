import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '../api'

export const useAuthStore = defineStore('auth', () => {
  const token = ref(localStorage.getItem('xr-hub-token') || '')
  const username = ref(localStorage.getItem('xr-hub-username') || '')

  async function login(user: string, password: string): Promise<void> {
    const resp = await api.login(user, password)
    token.value = resp.token
    username.value = resp.username
    localStorage.setItem('xr-hub-token', resp.token)
    localStorage.setItem('xr-hub-username', resp.username)
  }

  function logout() {
    token.value = ''
    username.value = ''
    localStorage.removeItem('xr-hub-token')
    localStorage.removeItem('xr-hub-username')
  }

  return { token, username, login, logout }
})
