<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useSharesStore } from '../stores/shares'
import { api } from '../api'
import type { CreateShareRequest, ShareRecord, ShareToken } from '../api'

const sharesStore = useSharesStore()

const showDialog = ref(false)
const toast = ref('')

// Register form fields.
const name = ref('')
const owner = ref('')
const addr = ref('')
const port = ref(8443)
const agentPubkey = ref('')
const comment = ref('')
const formError = ref('')

// Token modal.
const showTokenModal = ref(false)
const tokenTtlOption = ref('604800')
const tokenShare = ref<ShareRecord | null>(null)
const mintedToken = ref<ShareToken | null>(null)

// Install-command modal (no-hands agent setup via a registration token).
const showInstallModal = ref(false)
const regToken = ref<string | null>(null)
const regLoading = ref(false)

onMounted(() => {
  sharesStore.fetchList()
})

function showToast(msg: string) {
  toast.value = msg
  setTimeout(() => (toast.value = ''), 3000)
}

const hubBase = window.location.origin

function openInstall() {
  regToken.value = null
  showInstallModal.value = true
}

async function generateInstall() {
  regLoading.value = true
  try {
    const r = await api.createRegToken(3600)
    regToken.value = r.token
  } catch (e) {
    showToast('Не удалось выдать токен: ' + (e as Error).message)
  } finally {
    regLoading.value = false
  }
}

function linuxCmd(): string {
  return `curl -fsSL ${hubBase}/share/install.sh | sudo sh -s -- --token ${regToken.value}`
}
function windowsCmd(): string {
  return `$env:XR_TOKEN="${regToken.value}"; irm ${hubBase}/share/install.ps1 | iex`
}
function copyText(t: string) {
  navigator.clipboard.writeText(t)
  showToast('Команда скопирована')
}

function shortKey(key: string): string {
  return key.length > 12 ? key.slice(0, 10) + '…' : key
}

function formatDate(iso: string): string {
  return iso ? new Date(iso).toLocaleString() : '-'
}

function resetForm() {
  name.value = ''
  owner.value = ''
  addr.value = ''
  port.value = 8443
  agentPubkey.value = ''
  comment.value = ''
  formError.value = ''
}

function openCreate() {
  resetForm()
  showDialog.value = true
}

async function handleCreate() {
  formError.value = ''
  const req: CreateShareRequest = {
    name: name.value.trim(),
    owner: owner.value.trim(),
    addr: addr.value.trim(),
    port: port.value,
    agent_pubkey: agentPubkey.value.trim(),
    comment: comment.value.trim(),
  }
  try {
    await sharesStore.create(req)
    showDialog.value = false
    showToast('Share registered')
  } catch (e) {
    formError.value = (e as Error).message
  }
}

async function handleDelete(share: ShareRecord) {
  if (confirm(`Unregister "${share.name}"? The agent and its files are untouched, only the hub index entry is removed.`)) {
    await sharesStore.remove(share.share_id)
    showToast('Share unregistered')
  }
}

function openTokenModal(share: ShareRecord) {
  tokenShare.value = share
  mintedToken.value = null
  tokenTtlOption.value = '604800'
  showTokenModal.value = true
}

async function handleMint() {
  if (!tokenShare.value) return
  const ttl = parseInt(tokenTtlOption.value)
  mintedToken.value = await api.mintShareToken(tokenShare.value.share_id, ttl)
}

function copyToken() {
  if (mintedToken.value) {
    navigator.clipboard.writeText(JSON.stringify(mintedToken.value))
    showToast('Token copied')
  }
}

function copyPubkey(key: string) {
  navigator.clipboard.writeText(key)
  showToast('Pubkey copied')
}
</script>

<template>
  <div>
    <div class="page-header">
      <h2>Shares</h2>
      <div style="display:flex; gap:0.5rem">
        <button class="btn-primary" @click="openInstall">Команда установки агента</button>
        <button class="btn-sm" @click="openCreate">Register Share (вручную)</button>
      </div>
    </div>

    <p class="hint">
      The hub stores only the agent's <strong>address and identity key</strong>, never any files.
      Consumers fetch the listing and bytes straight from the agent; access is gated by a
      hub-signed token the agent verifies offline.
    </p>

    <!-- Install-command modal (no-hands agent setup) -->
    <div v-if="showInstallModal" class="dialog-overlay" @click.self="showInstallModal = false">
      <div class="dialog">
        <h3>Установка агента одной командой</h3>
        <p class="hint">
          Запусти команду на машине, которая будет раздавать файлы. Агент поставит
          службу и получит долгоживущий мандат хаба. После этого шарь любые пути
          (папки или отдельные файлы) командой
          <code>sudo xr-share share &lt;путь&gt;</code>.
        </p>
        <div v-if="!regToken">
          <button class="btn-primary" :disabled="regLoading" @click="generateInstall">
            {{ regLoading ? 'Генерация…' : 'Выдать токен и показать команду' }}
          </button>
        </div>
        <div v-else>
          <div class="field">
            <label>Linux (под root / sudo)</label>
            <code class="token-json">{{ linuxCmd() }}</code>
            <button class="btn-sm" @click="copyText(linuxCmd())">Копировать (Linux)</button>
          </div>
          <div class="field">
            <label>Windows (PowerShell от администратора)</label>
            <code class="token-json">{{ windowsCmd() }}</code>
            <button class="btn-sm" @click="copyText(windowsCmd())">Копировать (Windows)</button>
          </div>
          <p class="hint">Токен живёт 1 час, для другой машины выдай новый.</p>
        </div>
        <div class="dialog-actions">
          <button class="btn-sm" @click="showInstallModal = false">Закрыть</button>
        </div>
      </div>
    </div>

    <!-- Register dialog -->
    <div v-if="showDialog" class="dialog-overlay" @click.self="showDialog = false">
      <div class="dialog">
        <h3>Register Share</h3>

        <div class="field">
          <label>Name</label>
          <input v-model="name" placeholder="e.g. Family photos" />
        </div>
        <div class="field-row">
          <div class="field">
            <label>Address (host or IP)</label>
            <input v-model="addr" placeholder="203.0.113.7" />
          </div>
          <div class="field">
            <label>Port</label>
            <input v-model.number="port" type="number" min="1" max="65535" />
          </div>
        </div>
        <div class="field">
          <label>Agent public key (base64, 32 bytes)</label>
          <input v-model="agentPubkey" placeholder="pinned identity of the xr-share agent" />
        </div>
        <div class="field-row">
          <div class="field">
            <label>Owner</label>
            <input v-model="owner" placeholder="Optional" />
          </div>
          <div class="field">
            <label>Comment</label>
            <input v-model="comment" placeholder="Optional" />
          </div>
        </div>

        <p v-if="formError" class="form-error">{{ formError }}</p>

        <div class="dialog-actions">
          <button class="btn-primary" @click="handleCreate">Register</button>
          <button class="btn-sm" @click="showDialog = false">Cancel</button>
        </div>
      </div>
    </div>

    <!-- Token modal -->
    <div v-if="showTokenModal" class="dialog-overlay" @click.self="showTokenModal = false">
      <div class="dialog">
        <h3>Access token for "{{ tokenShare?.name }}"</h3>
        <p class="hint">
          Hand this token to a consumer out-of-band. They present it to the agent, which verifies
          it offline. The hub never sees the transfer.
        </p>

        <div class="field" v-if="!mintedToken">
          <label>Valid for</label>
          <select v-model="tokenTtlOption">
            <option value="3600">1 hour</option>
            <option value="86400">24 hours</option>
            <option value="604800">7 days</option>
            <option value="2592000">30 days</option>
          </select>
        </div>

        <div v-if="mintedToken" class="token-box">
          <code class="token-json">{{ JSON.stringify(mintedToken, null, 2) }}</code>
          <p class="token-exp">Expires: {{ new Date(mintedToken.exp * 1000).toLocaleString() }}</p>
        </div>

        <div class="dialog-actions">
          <button v-if="!mintedToken" class="btn-primary" @click="handleMint">Mint token</button>
          <button v-if="mintedToken" class="btn-primary" @click="copyToken">Copy token</button>
          <button class="btn-sm" @click="showTokenModal = false">Close</button>
        </div>
      </div>
    </div>

    <!-- Shares table -->
    <table class="data-table">
      <thead>
        <tr>
          <th>Name</th>
          <th>Address</th>
          <th>Agent key</th>
          <th>Owner</th>
          <th>Created</th>
          <th>Comment</th>
          <th>Actions</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="s in sharesStore.shares" :key="s.share_id">
          <td>{{ s.name }}</td>
          <td><code>{{ s.addr }}:{{ s.port }}</code></td>
          <td>
            <code class="clickable" :title="s.agent_pubkey" @click="copyPubkey(s.agent_pubkey)">
              {{ shortKey(s.agent_pubkey) }}
            </code>
          </td>
          <td>{{ s.owner || '-' }}</td>
          <td>{{ formatDate(s.created_at) }}</td>
          <td>{{ s.comment }}</td>
          <td class="actions">
            <button class="btn-sm" @click="openTokenModal(s)">Token</button>
            <button class="btn-sm btn-danger" @click="handleDelete(s)">Delete</button>
          </td>
        </tr>
        <tr v-if="!sharesStore.shares.length">
          <td colspan="7" class="empty">No shares registered yet</td>
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

.dialog-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.5);
  display: flex; align-items: center; justify-content: center; z-index: 100;
}
.dialog {
  background: var(--bg-card); border-radius: 8px; padding: 1.5rem;
  max-width: 520px; width: 90%; max-height: 90vh; overflow-y: auto; color: var(--text);
}
.dialog h3 { margin-bottom: 1rem; }

.field { margin-bottom: 0.75rem; }
.field label { display: block; font-size: 0.8rem; font-weight: 600; margin-bottom: 0.25rem; color: var(--text); }
.field input, .field select { width: 100%; padding: 0.5rem; border: 1px solid var(--border); border-radius: 4px; background: var(--bg-input); color: var(--text); }
.field-row { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; }
.form-error { color: var(--danger); font-size: 0.85rem; margin: 0.5rem 0; }
.dialog-actions { display: flex; gap: 0.5rem; margin-top: 1rem; }

.token-box { background: var(--bg-preview); border-radius: 6px; padding: 0.75rem; margin: 0.75rem 0; }
.token-json { display: block; white-space: pre-wrap; word-break: break-all; font-size: 0.75rem; color: var(--text); }
.token-exp { font-size: 0.8rem; color: var(--text-muted); margin-top: 0.5rem; }

.data-table { width: 100%; border-collapse: collapse; }
.data-table th, .data-table td { padding: 0.75rem 0.5rem; text-align: left; border-bottom: 1px solid var(--border-light); font-size: 0.875rem; }
.data-table th { font-weight: 600; color: var(--text-muted); font-size: 0.75rem; text-transform: uppercase; }
.data-table code { color: var(--text); }
.data-table code.clickable { cursor: pointer; }
.actions { white-space: nowrap; }

.btn-primary { padding: 0.5rem 1.5rem; background: var(--btn-bg); color: var(--btn-text); border: none; border-radius: 4px; cursor: pointer; }
.btn-sm { padding: 0.25rem 0.75rem; font-size: 0.8rem; border: 1px solid var(--border); background: transparent; color: var(--text); border-radius: 4px; cursor: pointer; margin-right: 0.25rem; }
.btn-danger { color: var(--danger); border-color: var(--danger); }
</style>
