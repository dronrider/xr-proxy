<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { usePresetsStore } from '../stores/presets'

const store = usePresetsStore()
const router = useRouter()
const toast = ref('')

onMounted(() => store.fetchList())

async function handleDelete(name: string) {
  if (confirm(`Delete preset "${name}"?`)) {
    await store.remove(name)
    showToast('Preset deleted')
  }
}

function showToast(msg: string) {
  toast.value = msg
  setTimeout(() => (toast.value = ''), 3000)
}

function formatDate(iso: string): string {
  return new Date(iso).toLocaleString()
}
</script>

<template>
  <div>
    <div class="page-header">
      <h2>Presets</h2>
      <button class="btn-primary" @click="router.push('/presets/new')">New Preset</button>
    </div>

    <p v-if="store.loading" class="muted">Loading...</p>

    <table v-else class="data-table">
      <thead>
        <tr>
          <th>Name</th>
          <th>Version</th>
          <th>Updated</th>
          <th>Rules</th>
          <th>Actions</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="p in store.summaries" :key="p.name">
          <td>
            <router-link :to="`/presets/${p.name}`">{{ p.name }}</router-link>
          </td>
          <td>{{ p.version }}</td>
          <td>{{ formatDate(p.updated_at) }}</td>
          <td>{{ p.rules_count }}</td>
          <td>
            <button class="btn-sm" @click="router.push(`/presets/${p.name}`)">Edit</button>
            <button class="btn-sm btn-danger" @click="handleDelete(p.name)">Delete</button>
          </td>
        </tr>
        <tr v-if="!store.summaries.length">
          <td colspan="5" class="empty">No presets yet</td>
        </tr>
      </tbody>
    </table>

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

.muted { color: var(--text-muted); }
.empty { text-align: center; color: var(--text-muted); }

.data-table {
  width: 100%;
  border-collapse: collapse;
}

.data-table th,
.data-table td {
  padding: 0.75rem 1rem;
  text-align: left;
  border-bottom: 1px solid var(--border-light);
}

.data-table th {
  font-weight: 600;
  color: var(--text-muted);
  font-size: 0.875rem;
  text-transform: uppercase;
}

.data-table a {
  color: var(--text);
  text-decoration: underline;
}

.btn-primary {
  padding: 0.5rem 1.5rem;
  background: var(--btn-bg);
  color: var(--btn-text);
  border: none;
  border-radius: 4px;
  cursor: pointer;
}

.btn-sm {
  padding: 0.25rem 0.75rem;
  font-size: 0.875rem;
  border: 1px solid var(--border);
  background: transparent;
  color: var(--text);
  border-radius: 4px;
  cursor: pointer;
  margin-right: 0.5rem;
}

.btn-danger {
  color: var(--danger);
  border-color: var(--danger);
}
</style>
