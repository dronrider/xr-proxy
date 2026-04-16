<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useInvitesStore } from '../stores/invites'
import { usePresetsStore } from '../stores/presets'
import { api } from '../api'
import type { CreateInviteRequest, Invite, InviteDefaultsResponse } from '../api'
import QRCode from 'qrcode'

const invitesStore = useInvitesStore()
const presetsStore = usePresetsStore()

const showDialog = ref(false)
const toast = ref('')

// QR modal
const qrDataUrl = ref('')
const qrLink = ref('')
const showQrModal = ref(false)

// Defaults from server config
const defaults = ref<InviteDefaultsResponse | null>(null)

// Form fields
const preset = ref('')
const ttlOption = ref('86400')
const customTtlHours = ref(48)
const oneTime = ref(true)
const comment = ref('')

onMounted(async () => {
  invitesStore.fetchList()
  presetsStore.fetchList()
  try {
    defaults.value = await api.getInviteDefaults()
  } catch {
    // defaults not configured
  }
})

function showToast(msg: string) {
  toast.value = msg
  setTimeout(() => (toast.value = ''), 3000)
}

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

function inviteUrl(invite: Invite): string {
  const base = invite.payload.hub_url || window.location.origin
  return `${base}/api/v1/invite/${invite.token}/view`
}

async function showQr(invite: Invite) {
  const url = inviteUrl(invite)
  qrLink.value = url
  qrDataUrl.value = await QRCode.toDataURL(url, { width: 256 })
  showQrModal.value = true
}

async function handleCreate() {
  const ttl = ttlOption.value === 'custom'
    ? customTtlHours.value * 3600
    : parseInt(ttlOption.value)
  const req: CreateInviteRequest = {
    ttl_seconds: ttl,
    one_time: oneTime.value,
    comment: comment.value,
    preset: preset.value,
  }
  const invite = await invitesStore.create(req)
  showDialog.value = false
  showToast('Invite created')

  // Show QR for newly created invite
  await showQr(invite)
}

async function handleRevoke(token: string) {
  if (confirm('Revoke this invite?')) {
    await invitesStore.revoke(token)
    showToast('Invite revoked')
  }
}

function copyLink(invite: Invite) {
  navigator.clipboard.writeText(inviteUrl(invite))
  showToast('Link copied')
}
</script>

<template>
  <div>
    <div class="page-header">
      <h2>Invites</h2>
      <button class="btn-primary" @click="showDialog = true">New Invite</button>
    </div>

    <!-- QR Modal -->
    <div v-if="showQrModal" class="dialog-overlay" @click.self="showQrModal = false">
      <div class="dialog qr-dialog">
        <h3>Invite QR Code</h3>
        <img v-if="qrDataUrl" :src="qrDataUrl" alt="QR" class="qr-image" />
        <div class="qr-link-row">
          <code class="qr-link">{{ qrLink }}</code>
        </div>
        <button class="btn-sm" @click="showQrModal = false">Close</button>
      </div>
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
            <label>TTL</label>
            <select v-model="ttlOption">
              <option value="3600">1 hour</option>
              <option value="86400">24 hours</option>
              <option value="604800">7 days</option>
              <option value="custom">Custom...</option>
            </select>
          </div>
          <div class="field" v-if="ttlOption === 'custom'">
            <label>Hours</label>
            <input v-model.number="customTtlHours" type="number" min="1" max="8760" placeholder="48" />
          </div>
          <div class="field" v-else>
            <label class="checkbox-label">
              <input type="checkbox" v-model="oneTime" />
              One-time
            </label>
          </div>
        </div>
        <div class="field-row" v-if="ttlOption === 'custom'">
          <div class="field">
            <label class="checkbox-label">
              <input type="checkbox" v-model="oneTime" />
              One-time
            </label>
          </div>
        </div>

        <div class="field">
          <label>Comment</label>
          <input v-model="comment" placeholder="Optional" />
        </div>

        <p v-if="defaults" class="defaults-hint">
          Server: {{ defaults.server_address }}:{{ defaults.server_port }} | Preset: {{ preset || '—' }}
        </p>

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
          <th>Claimed by</th>
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
          <td><code v-if="inv.claimed_by_ip">{{ inv.claimed_by_ip }}</code></td>
          <td>{{ inv.comment }}</td>
          <td class="actions">
            <button
              v-if="statusOf(inv) === 'active'"
              class="btn-sm"
              @click="showQr(inv)"
            >QR</button>
            <button
              v-if="statusOf(inv) === 'active'"
              class="btn-sm"
              @click="copyLink(inv)"
            >Copy</button>
            <button
              v-if="statusOf(inv) === 'active'"
              class="btn-sm btn-danger"
              @click="handleRevoke(inv.token)"
            >Revoke</button>
          </td>
        </tr>
        <tr v-if="!invitesStore.invites.length">
          <td colspan="8" class="empty">No invites yet</td>
        </tr>
      </tbody>
    </table>

    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>

<style scoped>
.page-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 1.5rem; }
.empty { text-align: center; color: var(--text-muted); }

.dialog-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.5);
  display: flex; align-items: center; justify-content: center; z-index: 100;
}

.dialog {
  background: var(--bg-card); border-radius: 8px; padding: 1.5rem;
  max-width: 460px; width: 90%; max-height: 90vh; overflow-y: auto;
  color: var(--text);
}

.dialog h3 { margin-bottom: 1rem; }

.qr-dialog { text-align: center; }
.qr-image { display: block; margin: 1rem auto; border-radius: 8px; }
.qr-link-row { margin-bottom: 1rem; }
.qr-link { font-size: 0.75rem; word-break: break-all; color: var(--text-muted); }

.field { margin-bottom: 0.75rem; }
.field label { display: block; font-size: 0.8rem; font-weight: 600; margin-bottom: 0.25rem; color: var(--text); }
.field input, .field select { width: 100%; padding: 0.5rem; border: 1px solid var(--border); border-radius: 4px; background: var(--bg-input); color: var(--text); }
.checkbox-label { display: flex; align-items: center; gap: 0.5rem; margin-top: 1.5rem; }
.checkbox-label input[type="checkbox"] { width: auto; }
.field-row { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; }
.defaults-hint { font-size: 0.8rem; color: var(--text-muted); margin: 0.5rem 0; }
.dialog-actions { display: flex; gap: 0.5rem; margin-top: 1rem; }

.data-table { width: 100%; border-collapse: collapse; }
.data-table th, .data-table td { padding: 0.75rem 0.5rem; text-align: left; border-bottom: 1px solid var(--border-light); font-size: 0.875rem; }
.data-table th { font-weight: 600; color: var(--text-muted); font-size: 0.75rem; text-transform: uppercase; }
.data-table code { color: var(--text); }
.actions { white-space: nowrap; }

.btn-primary { padding: 0.5rem 1.5rem; background: var(--btn-bg); color: var(--btn-text); border: none; border-radius: 4px; cursor: pointer; }
.btn-sm { padding: 0.25rem 0.75rem; font-size: 0.8rem; border: 1px solid var(--border); background: transparent; color: var(--text); border-radius: 4px; cursor: pointer; margin-right: 0.25rem; }
.btn-danger { color: var(--danger); border-color: var(--danger); }

.status-active { color: var(--success); font-weight: 600; }
.status-expired { color: var(--text-muted); }
.status-consumed { color: var(--warning); }
</style>
