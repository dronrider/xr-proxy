import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from './stores/auth'

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: '/',
      redirect: '/presets',
    },
    {
      path: '/login',
      name: 'Login',
      component: () => import('./pages/Login.vue'),
    },
    {
      path: '/presets',
      name: 'PresetsList',
      component: () => import('./pages/PresetsList.vue'),
    },
    {
      path: '/presets/new',
      name: 'PresetCreate',
      component: () => import('./pages/PresetEdit.vue'),
    },
    {
      path: '/presets/:name',
      name: 'PresetEdit',
      component: () => import('./pages/PresetEdit.vue'),
    },
    {
      path: '/invites',
      name: 'InvitesList',
      component: () => import('./pages/InvitesList.vue'),
    },
  ],
})

// Track whether session has been validated this app lifecycle.
let sessionChecked = false

router.beforeEach(async (to) => {
  const auth = useAuthStore()

  if (to.name === 'Login') return

  if (!auth.token) {
    return { name: 'Login' }
  }

  // On first navigation, verify the session is still valid on the server.
  if (!sessionChecked) {
    try {
      const resp = await fetch('/api/v1/admin/invites', {
        headers: { Authorization: `Bearer ${auth.token}` },
      })
      if (resp.status === 401) {
        auth.logout()
        return { name: 'Login' }
      }
      sessionChecked = true
    } catch {
      // Network error — let through, will fail on actual requests.
      sessionChecked = true
    }
  }
})

export default router
