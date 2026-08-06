<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useExposesStore } from '../stores/exposes'
import type { ExposeRecord } from '../api'

const exposesStore = useExposesStore()
const toast = ref('')

onMounted(() => {
  exposesStore.fetchList()
})

function showToast(msg: string) {
  toast.value = msg
  setTimeout(() => (toast.value = ''), 3000)
}

function shortKey(key: string): string {
  return key.length > 12 ? key.slice(0, 10) + '...' : key
}

function formatDate(iso: string): string {
  return iso ? new Date(iso).toLocaleString() : '-'
}

function copyPubkey(key: string) {
  navigator.clipboard.writeText(key)
  showToast('Ключ агента скопирован')
}

async function handleDelete(rec: ExposeRecord) {
  const ok = confirm(
    `Снять публикацию "${rec.name}"? Поддомен освободится, локальный сервис на машине агента не трогается.`,
  )
  if (!ok) return
  await exposesStore.remove(rec.name)
  showToast('Публикация снята')
}
</script>

<template>
  <div>
    <div class="page-header">
      <h2>Публикации</h2>
      <button class="btn-sm" @click="exposesStore.fetchList()">Обновить</button>
    </div>

    <p class="hint">
      Публикация это локальный HTTP-сервис на машине владельца, выведенный наружу браузерным
      входом. Хаб помнит только имя (оно же поддомен) и ключ агента: адрес сервиса остаётся в
      конфиге агента. Заводит публикацию сам агент командой
      <code>xr-share expose add --name &lt;имя&gt;</code>, здесь её видно и можно снять, даже
      когда машина не на связи. На браузерном пути трафик расшифровывается на сервере входа.
    </p>

    <table class="data-table">
      <thead>
        <tr>
          <th>Имя</th>
          <th>Агент</th>
          <th>Заведена</th>
          <th>Действия</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="e in exposesStore.exposes" :key="e.name">
          <td><code>{{ e.name }}</code></td>
          <td>
            <code class="clickable" :title="e.agent_pubkey" @click="copyPubkey(e.agent_pubkey)">
              {{ shortKey(e.agent_pubkey) }}
            </code>
          </td>
          <td>{{ formatDate(e.created) }}</td>
          <td class="actions">
            <button class="btn-sm btn-danger" @click="handleDelete(e)">Снять</button>
          </td>
        </tr>
        <tr v-if="!exposesStore.loading && exposesStore.exposes.length === 0">
          <td colspan="4" class="empty">Публикаций нет</td>
        </tr>
      </tbody>
    </table>

    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>

<style scoped>
.page-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 1rem; }
.hint { font-size: 0.85rem; color: var(--text-muted); margin-bottom: 1.5rem; line-height: 1.4; }
.empty { text-align: center; color: var(--text-muted); }

.data-table { width: 100%; border-collapse: collapse; }
.data-table th, .data-table td { padding: 0.75rem 0.5rem; text-align: left; border-bottom: 1px solid var(--border-light); font-size: 0.875rem; }
.data-table th { font-weight: 600; color: var(--text-muted); font-size: 0.75rem; text-transform: uppercase; }
.data-table code { color: var(--text); }
.data-table code.clickable { cursor: pointer; }
.actions { white-space: nowrap; }

.btn-sm { padding: 0.25rem 0.75rem; font-size: 0.8rem; border: 1px solid var(--border); background: transparent; color: var(--text); border-radius: 4px; cursor: pointer; margin-right: 0.25rem; }
.btn-danger { color: var(--danger); border-color: var(--danger); }
</style>
