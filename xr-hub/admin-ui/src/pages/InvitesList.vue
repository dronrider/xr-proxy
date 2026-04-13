<script setup lang="ts">
import { ref, onMounted, computed } from 'vue'
import { useInvitesStore } from '../stores/invites'
import { usePresetsStore } from '../stores/presets'
import type { CreateInviteRequest, Invite } from '../api'
import QRCode from 'qrcode'

const invitesStore = useInvitesStore()
const presetsStore = usePresetsStore()

const showDialog = ref(false)
const createdInvite = ref<Invite | null>(null)
const qrDataUrl = ref('')

// Form fields
const serverAddress = ref('')
const serverPort = ref(8443)
const obfuscationKey = ref('')
const modifier = ref('positional_xor_rotate')
const salt = ref(0)
const preset = ref('')
const hubUrl = ref('')
const ttlOption = ref('86400')
const oneTime = ref(true)
const comment = ref('')

onMounted(() => {
  invitesStore.fetchList()
  presetsStore.fetchList()
})

function statusOf(invite: Invite): string {
  if (invite.consumed_at) return 'consumed'
  const now = new Date().toISOString()
  if (invite.expires_at <= now) return 'expired'
  return 'active'
}

function statusClass(invite: Invite): string {
  return `status-${statusOf(invite)}`
}

function shortToken(token: string): string {
  return token.slice(0, 8) + '...'
}

function formatDate(iso: string): string {
  return new Date(iso).toLocaleString()
}

const inviteUrl = computed(() => {
  if (!createdInvite.value) return ''
  const base = hubUrl.value || window.location.origin
  return `${base}/api/v1/invite/${createdInvite.value.token}`
})

async function handleCreate() {
  const ttl = parseInt(ttlOption.value)
  const req: CreateInviteRequest = {
    ttl_seconds: ttl,
    one_time: oneTime.value,
    comment: comment.value,
    payload: {
      server_address: serverAddress.value,
      server_port: serverPort.value,
      obfuscation_key: obfuscationKey.value,
      modifier: modifier.value,
      salt: salt.value,
      preset: preset.value,
      hub_url: hubUrl.value || window.location.origin,
    },
  }
  const invite = await invitesStore.create(req)
  createdInvite.value = invite
  showDialog.value = false

  // Generate QR
  const url = `${req.payload.hub_url}/api/v1/invite/${invite.token}`
  qrDataUrl.value = await QRCode.toDataURL(url, { width: 256 })
}

async function handleRevoke(token: string) {
  if (confirm('Revoke this invite?')) {
    await invitesStore.revoke(token)
  }
}

function copyLink() {
  navigator.clipboard.writeText(inviteUrl.value)
}

function closeResult() {
  createdInvite.value = null
  qrDataUrl.value = ''
}
</script>

<template>
  <div>
    <div class="page-header">
      <h2>Invites</h2>
      <button class="btn-primary" @click="showDialog = true">New Invite</button>
    </div>

    <!-- Created invite result -->
    <div v-if="createdInvite" class="invite-result">
      <h3>Invite Created</h3>
      <div class="result-row">
        <code>{{ inviteUrl }}</code>
        <button class="btn-sm" @click="copyLink">Copy</button>
      </div>
      <img v-if="qrDataUrl" :src="qrDataUrl" alt="QR" class="qr-image" />
      <button class="btn-sm" @click="closeResult">Close</button>
    </div>

    <!-- Create dialog -->
    <div v-if="showDialog" class="dialog-overlay" @click.self="showDialog = false">
      <div class="dialog">
        <h3>Create Invite</h3>

        <div class="field">
          <label>Preset</label>
          <select v-model="preset">
            <option v-for="p in presetsStore.summaries" :key="p.name" :value="p.name">
              {{ p.name }}
            </option>
          </select>
        </div>

        <div class="field-row">
          <div class="field">
            <label>Server Address</label>
            <input v-model="serverAddress" placeholder="1.2.3.4" />
          </div>
          <div class="field">
            <label>Port</label>
            <input v-model.number="serverPort" type="number" />
          </div>
        </div>

        <div class="field">
          <label>Obfuscation Key (base64)</label>
          <input v-model="obfuscationKey" />
        </div>

        <div class="field-row">
          <div class="field">
            <label>Modifier</label>
            <input v-model="modifier" />
          </div>
          <div class="field">
            <label>Salt</label>
            <input v-model.number="salt" type="number" />
          </div>
        </div>

        <div class="field">
          <label>Hub URL</label>
          <input v-model="hubUrl" placeholder="https://xr-hub.example.com" />
        </div>

        <div class="field-row">
          <div class="field">
            <label>TTL</label>
            <select v-model="ttlOption">
              <option value="3600">1 hour</option>
              <option value="86400">24 hours</option>
              <option value="604800">7 days</option>
            </select>
          </div>
          <div class="field">
            <label>
              <input type="checkbox" v-model="oneTime" />
              One-time
            </label>
          </div>
        </div>

        <div class="field">
          <label>Comment</label>
          <input v-model="comment" placeholder="Optional" />
        </div>

        <div class="dialog-actions">
          <button class="btn-primary" @click="handleCreate">Create</button>
          <button class="btn-sm" @click="showDialog = false">Cancel</button>
        </div>
      </div>
    </div>

    <!-- Invites table -->
    <table class="data-table">
      <thead>
        <tr>
          <th>Token</th>
          <th>Preset</th>
          <th>Created</th>
          <th>Expires</th>
          <th>Status</th>
          <th>Comment</th>
          <th>Actions</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="inv in invitesStore.invites" :key="inv.token">
          <td><code>{{ shortToken(inv.token) }}</code></td>
          <td>{{ inv.payload.preset }}</td>
          <td>{{ formatDate(inv.created_at) }}</td>
          <td>{{ formatDate(inv.expires_at) }}</td>
          <td><span :class="statusClass(inv)">{{ statusOf(inv) }}</span></td>
          <td>{{ inv.comment }}</td>
          <td>
            <button
              v-if="statusOf(inv) === 'active'"
              class="btn-sm btn-danger"
              @click="handleRevoke(inv.token)"
            >
              Revoke
            </button>
          </td>
        </tr>
        <tr v-if="!invitesStore.invites.length">
          <td colspan="7" style="text-align: center; color: #999">No invites yet</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.page-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 1.5rem;
}

.invite-result {
  margin-bottom: 1.5rem;
  padding: 1rem;
  background: #e8f5e9;
  border-radius: 8px;
}

.invite-result h3 {
  margin-bottom: 0.5rem;
}

.result-row {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  margin-bottom: 0.5rem;
}

.result-row code {
  font-size: 0.8rem;
  word-break: break-all;
}

.qr-image {
  display: block;
  margin: 0.5rem 0;
}

.dialog-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.4);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}

.dialog {
  background: #fff;
  border-radius: 8px;
  padding: 1.5rem;
  max-width: 500px;
  width: 90%;
  max-height: 90vh;
  overflow-y: auto;
}

.dialog h3 {
  margin-bottom: 1rem;
}

.field {
  margin-bottom: 0.75rem;
}

.field label {
  display: block;
  font-size: 0.8rem;
  font-weight: 600;
  margin-bottom: 0.25rem;
}

.field input,
.field select {
  width: 100%;
  padding: 0.5rem;
  border: 1px solid #ccc;
  border-radius: 4px;
}

.field-row {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0.75rem;
}

.dialog-actions {
  display: flex;
  gap: 0.5rem;
  margin-top: 1rem;
}

.data-table {
  width: 100%;
  border-collapse: collapse;
}

.data-table th,
.data-table td {
  padding: 0.75rem 0.5rem;
  text-align: left;
  border-bottom: 1px solid #eee;
  font-size: 0.875rem;
}

.data-table th {
  font-weight: 600;
  color: #666;
  font-size: 0.75rem;
  text-transform: uppercase;
}

.btn-primary {
  padding: 0.5rem 1.5rem;
  background: #1a1a2e;
  color: #fff;
  border: none;
  border-radius: 4px;
  cursor: pointer;
}

.btn-sm {
  padding: 0.25rem 0.75rem;
  font-size: 0.875rem;
  border: 1px solid #ccc;
  background: #fff;
  border-radius: 4px;
  cursor: pointer;
}

.btn-danger {
  color: #d32f2f;
  border-color: #d32f2f;
}

.status-active {
  color: #2e7d32;
  font-weight: 600;
}

.status-expired {
  color: #999;
}

.status-consumed {
  color: #f57c00;
}
</style>
