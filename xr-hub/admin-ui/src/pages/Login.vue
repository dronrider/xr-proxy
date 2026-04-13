<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { useAuthStore } from '../stores/auth'

const auth = useAuthStore()
const router = useRouter()

const username = ref('')
const password = ref('')
const error = ref('')
const loading = ref(false)

async function login() {
  error.value = ''
  loading.value = true
  try {
    await auth.login(username.value, password.value)
    router.push('/presets')
  } catch (e: any) {
    if (e.message?.includes('401')) {
      error.value = 'Invalid username or password'
    } else {
      error.value = 'Connection error'
    }
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div class="login-page">
    <div class="login-card">
      <h2>xr-hub</h2>
      <p class="subtitle">Admin Login</p>
      <form @submit.prevent="login">
        <div class="field">
          <label>Username</label>
          <input
            v-model="username"
            type="text"
            autocomplete="username"
            :disabled="loading"
          />
        </div>
        <div class="field">
          <label>Password</label>
          <input
            v-model="password"
            type="password"
            autocomplete="current-password"
            :disabled="loading"
          />
        </div>
        <p v-if="error" class="error">{{ error }}</p>
        <button type="submit" :disabled="loading || !username || !password">
          {{ loading ? 'Logging in...' : 'Login' }}
        </button>
      </form>
    </div>
  </div>
</template>

<style scoped>
.login-page {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 60vh;
}

.login-card {
  width: 100%;
  max-width: 360px;
}

.login-card h2 {
  font-size: 1.75rem;
  color: var(--text-heading);
  margin-bottom: 0.25rem;
}

.subtitle {
  color: var(--text-muted);
  margin-bottom: 2rem;
}

.field {
  margin-bottom: 1rem;
}

.field label {
  display: block;
  margin-bottom: 0.5rem;
  font-weight: 600;
  color: var(--text);
}

.field input {
  width: 100%;
  padding: 0.75rem;
  border: 1px solid var(--border);
  border-radius: 4px;
  font-size: 1rem;
  background: var(--bg-input);
  color: var(--text);
}

.error {
  color: var(--danger);
  margin-bottom: 1rem;
  font-size: 0.9rem;
}

button {
  width: 100%;
  padding: 0.75rem;
  background: var(--btn-bg);
  color: var(--btn-text);
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
