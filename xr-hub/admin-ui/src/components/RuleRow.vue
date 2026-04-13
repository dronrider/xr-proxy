<script setup lang="ts">
import { computed } from 'vue'
import type { RoutingRule } from '../api'

const props = defineProps<{ rule: RoutingRule }>()
const emit = defineEmits<{
  update: [rule: RoutingRule]
  remove: []
}>()

const domainsText = computed({
  get: () => props.rule.domains.join('\n'),
  set: (v: string) =>
    emit('update', {
      ...props.rule,
      domains: v
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean),
    }),
})

const ipRangesText = computed({
  get: () => props.rule.ip_ranges.join('\n'),
  set: (v: string) =>
    emit('update', {
      ...props.rule,
      ip_ranges: v
        .split('\n')
        .map((s) => s.trim())
        .filter(Boolean),
    }),
})

const geoipText = computed({
  get: () => props.rule.geoip.join(', '),
  set: (v: string) =>
    emit('update', {
      ...props.rule,
      geoip: v
        .split(',')
        .map((s) => s.trim().toUpperCase())
        .filter(Boolean),
    }),
})
</script>

<template>
  <div class="rule-row">
    <div class="row-header">
      <select
        :value="rule.action"
        @change="emit('update', { ...rule, action: ($event.target as HTMLSelectElement).value })"
      >
        <option value="proxy">proxy</option>
        <option value="direct">direct</option>
      </select>
      <button type="button" class="btn-remove" @click="emit('remove')">Remove</button>
    </div>

    <div class="row-fields">
      <div class="field">
        <label>Domains (one per line)</label>
        <textarea v-model="domainsText" rows="3" placeholder="youtube.com&#10;*.google.com"></textarea>
      </div>
      <div class="field">
        <label>IP Ranges (CIDR, one per line)</label>
        <textarea v-model="ipRangesText" rows="3" placeholder="91.108.56.0/22"></textarea>
      </div>
      <div class="field">
        <label>GeoIP (comma-separated)</label>
        <input v-model="geoipText" placeholder="US, NL, DE" />
      </div>
    </div>
  </div>
</template>

<style scoped>
.row-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 0.75rem;
}

.row-header select {
  padding: 0.25rem 0.5rem;
  border: 1px solid #ccc;
  border-radius: 4px;
}

.btn-remove {
  padding: 0.25rem 0.5rem;
  font-size: 0.8rem;
  color: #d32f2f;
  border: 1px solid #d32f2f;
  background: transparent;
  border-radius: 4px;
  cursor: pointer;
}

.row-fields {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0.75rem;
}

.field label {
  display: block;
  font-size: 0.75rem;
  color: #666;
  margin-bottom: 0.25rem;
}

.field textarea,
.field input {
  width: 100%;
  padding: 0.5rem;
  border: 1px solid #ccc;
  border-radius: 4px;
  font-family: monospace;
  font-size: 0.8rem;
}

.field:last-child {
  grid-column: 1 / -1;
}
</style>
