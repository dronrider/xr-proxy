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

router.beforeEach((to) => {
  const auth = useAuthStore()
  if (to.name !== 'Login' && !auth.token) {
    return { name: 'Login' }
  }
})

export default router
