import { defineStore } from 'pinia'
import { ref } from 'vue'

export const useAuthStore = defineStore('auth', () => {
  const token = ref(localStorage.getItem('xr-hub-token') || '')

  function setToken(t: string) {
    token.value = t
    localStorage.setItem('xr-hub-token', t)
  }

  function logout() {
    token.value = ''
    localStorage.removeItem('xr-hub-token')
  }

  return { token, setToken, logout }
})
