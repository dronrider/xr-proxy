<script setup lang="ts">
import { RouterView } from 'vue-router'
</script>

<template>
  <div class="app-container">
    <header class="app-header">
      <h1>xr-hub</h1>
      <nav>
        <router-link to="/presets">Presets</router-link>
        <router-link to="/invites">Invites</router-link>
      </nav>
    </header>
    <main>
      <RouterView />
    </main>
  </div>
</template>

<style>
* {
  margin: 0;
  padding: 0;
  box-sizing: border-box;
}

body {
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
  background: #f5f5f5;
  color: #333;
}

.app-container {
  max-width: 1200px;
  margin: 0 auto;
  padding: 1rem;
}

.app-header {
  display: flex;
  align-items: center;
  gap: 2rem;
  margin-bottom: 2rem;
  padding: 1rem;
  background: #fff;
  border-radius: 8px;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
}

.app-header h1 {
  font-size: 1.5rem;
  color: #1a1a2e;
}

.app-header nav {
  display: flex;
  gap: 1rem;
}

.app-header nav a {
  color: #555;
  text-decoration: none;
  padding: 0.5rem 1rem;
  border-radius: 4px;
  transition: background 0.2s;
}

.app-header nav a:hover,
.app-header nav a.router-link-active {
  background: #e8eaf6;
  color: #1a1a2e;
}

main {
  background: #fff;
  border-radius: 8px;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
  padding: 1.5rem;
}
</style>
