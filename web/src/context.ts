import { createContext } from 'preact'
import { useContext } from 'preact/hooks'
import type { ReadonlySignal } from '@preact/signals'
import type { Status } from './connection'
import type { MailStore } from './stores/mail'

export interface Services {
  mail: MailStore
  status: ReadonlySignal<Status>
}

export const ServicesContext = createContext<Services | null>(null)

export function useServices(): Services {
  const services = useContext(ServicesContext)
  if (!services) throw new Error('useServices outside ServicesContext')
  return services
}
