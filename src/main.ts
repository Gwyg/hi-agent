import { createApp } from 'vue'
import ElementPlus from 'element-plus'
import 'element-plus/dist/index.css'
// 放在 element-plus 之后:确保我们的 html/body/#app reset 生效,不被组件库样式覆盖
import './styles/global.css'
import App from './App.vue'
import router from './router'

createApp(App).use(router).use(ElementPlus).mount('#app')
