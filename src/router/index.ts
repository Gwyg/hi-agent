import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'

import ChatView from '../views/ChatView.vue'

const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/chat' },
  { path: '/chat', name: 'chat', component: ChatView },
]

const router = createRouter({
  history: createWebHashHistory(),
  routes,
})

export default router
