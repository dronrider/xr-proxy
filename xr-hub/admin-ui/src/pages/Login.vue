<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '../stores/auth'
import { api } from '../api'

const auth = useAuthStore()
const router = useRouter()

const token = ref('')
const error = ref('')
const loading = ref(false)

async function login() {
  error.value = ''
  loading.value = true
  try {
    auth.setToken(token.value)
    const ok = await api.testAuth()
    if (ok) {
      router.push('/presets')
    } else {
      error.value = 'Invalid token'
      auth.logout()
    }
  } catch (e) {
    error.value = 'Connection error'
    auth.logout()
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div class="login-page">
    <h2>Admin Login</h2>
    <form @submit.prevent="login">
      <div class="field">
        <label>Bearer Token</label>
        <input
          v-model="token"
          type="password"
          placeholder="Enter admin token"
          :disabled="loading"
        />
      </div>
      <p v-if="error" class="error">{{ error }}</p>
      <button type="submit" :disabled="loading || !token">
        {{ loading ? 'Checking...' : 'Login' }}
      </button>
    </form>
  </div>
</template>

<style scoped>
.login-page {
  max-width: 400px;
  margin: 4rem auto;
}

.field {
  margin-bottom: 1rem;
}

.field label {
  display: block;
  margin-bottom: 0.5rem;
  font-weight: 600;
}

.field input {
  width: 100%;
  padding: 0.75rem;
  border: 1px solid #ccc;
  border-radius: 4px;
  font-size: 1rem;
}

.error {
  color: #d32f2f;
  margin-bottom: 1rem;
}

button {
  padding: 0.75rem 2rem;
  background: #1a1a2e;
  color: #fff;
  border: none;
  border-radius: 4px;
  cursor: pointer;
  font-size: 1rem;
}

button:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>
