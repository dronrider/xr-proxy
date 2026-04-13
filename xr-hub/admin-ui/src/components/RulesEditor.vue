<script setup lang="ts">
import type { RoutingRule } from '../api'
import RuleRow from './RuleRow.vue'

const rules = defineModel<RoutingRule[]>({ required: true })

function addRule() {
  rules.value = [
    ...rules.value,
    { action: 'proxy', domains: [], ip_ranges: [], geoip: [] },
  ]
}

function removeRule(index: number) {
  rules.value = rules.value.filter((_, i) => i !== index)
}

function updateRule(index: number, rule: RoutingRule) {
  const copy = [...rules.value]
  copy[index] = rule
  rules.value = copy
}
</script>

<template>
  <div class="rules-editor">
    <h3>Rules</h3>
    <div v-for="(rule, i) in rules" :key="i" class="rule-item">
      <RuleRow :rule="rule" @update="(r) => updateRule(i, r)" @remove="removeRule(i)" />
    </div>
    <button type="button" class="btn-add" @click="addRule">+ Add Rule</button>
  </div>
</template>

<style scoped>
.rules-editor h3 {
  margin-bottom: 0.5rem;
  font-size: 0.875rem;
  color: #666;
  text-transform: uppercase;
}

.rule-item {
  margin-bottom: 1rem;
  padding: 1rem;
  border: 1px solid #e0e0e0;
  border-radius: 4px;
}

.btn-add {
  padding: 0.5rem 1rem;
  border: 1px dashed #ccc;
  background: transparent;
  border-radius: 4px;
  cursor: pointer;
  color: #666;
}

.btn-add:hover {
  border-color: #999;
  color: #333;
}
</style>
