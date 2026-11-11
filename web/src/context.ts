import { createContext } from 'preact'
import { useContext } from 'preact/hooks'
import type { ReadonlySignal } from '@preact/signals'
import type { Status } from './connection'
import type { CalendarStore } from './stores/calendar'
import type { DraftStore } from './stores/drafts'
import type { HistoryStore } from './stores/history'
import type { HomeStore } from './stores/home'
import type { MailStore } from './stores/mail'
import type { MatrixStore } from './stores/matrix'
import type { Ops } from './stores/ops'
import type { TodoStore } from './stores/todos'

export interface Services {
  ops: Ops
  mail: MailStore
  drafts: DraftStore
  matrix: MatrixStore
  todos: TodoStore
  history: HistoryStore
  calendar: CalendarStore
  home: HomeStore
  status: ReadonlySignal<Status>
}

export const ServicesContext = createContext<Services | null>(null)

export function useServices(): Services {
  const services = useContext(ServicesContext)
  if (!services) throw new Error('useServices outside ServicesContext')
  return services
}
