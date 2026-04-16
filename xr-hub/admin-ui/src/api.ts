const BASE = '/api/v1'

function getToken(): string {
  return localStorage.getItem('xr-hub-token') || ''
}

async function request<T>(path: string, options: RequestInit = {}): Promise<T> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...((options.headers as Record<string, string>) || {}),
  }

  const token = getToken()
  if (token) {
    headers['Authorization'] = `Bearer ${token}`
  }

  const resp = await fetch(`${BASE}${path}`, {
    ...options,
    headers,
  })

  if (!resp.ok) {
    if (resp.status === 401 && !path.includes('/auth/login')) {
      // Session expired — clear and redirect to login.
      localStorage.removeItem('xr-hub-token')
      localStorage.removeItem('xr-hub-username')
      window.location.href = '/login'
      throw new Error('Session expired')
    }
    const text = await resp.text()
    throw new Error(`${resp.status}: ${text}`)
  }

  if (resp.status === 204) {
    return undefined as unknown as T
  }

  return resp.json()
}

export const api = {
  // Auth
  login: (username: string, password: string) =>
    request<LoginResponse>('/auth/login', {
      method: 'POST',
      body: JSON.stringify({ username, password }),
    }),

  // Public
  listPresets: () => request<PresetSummary[]>('/presets'),
  getPreset: (name: string) => request<Preset>(`/presets/${name}`),
  getInviteInfo: (token: string) => request<InviteInfo>(`/invite/${token}`),
  claimInvite: (token: string) =>
    request<InvitePayload>(`/invite/${token}/claim`, { method: 'POST' }),

  // Admin presets
  createPreset: (data: CreatePresetRequest) =>
    request<Preset>('/admin/presets', {
      method: 'POST',
      body: JSON.stringify(data),
    }),
  updatePreset: (name: string, data: CreatePresetRequest) =>
    request<Preset>(`/admin/presets/${name}`, {
      method: 'PUT',
      body: JSON.stringify(data),
    }),
  deletePreset: (name: string) =>
    request<void>(`/admin/presets/${name}`, { method: 'DELETE' }),

  // Admin invites
  listInvites: () => request<Invite[]>('/admin/invites'),
  getInviteDefaults: () => request<InviteDefaultsResponse>('/admin/invite-defaults'),
  createInvite: (data: CreateInviteRequest) =>
    request<Invite>('/admin/invites', {
      method: 'POST',
      body: JSON.stringify(data),
    }),
  revokeInvite: (token: string) =>
    request<void>(`/admin/invites/${token}`, { method: 'DELETE' }),
}

// Types
export interface LoginResponse {
  token: string
  username: string
}

export interface RoutingRule {
  action: string
  domains: string[]
  ip_ranges: string[]
  geoip: string[]
}

export interface RoutingConfig {
  default_action: string
  rules: RoutingRule[]
}

export interface Preset {
  name: string
  version: number
  updated_at: string
  description: string
  rules: RoutingConfig
  signature?: string
}

export interface PresetSummary {
  name: string
  version: number
  updated_at: string
  rules_count: number
}

export interface InvitePayload {
  server_address: string
  server_port: number
  obfuscation_key: string
  modifier: string
  salt: number
  preset: string
  hub_url: string
}

export interface InviteInfo {
  token: string
  preset: string
  comment: string
  status: string
  expires_at: string
}

export interface Invite {
  token: string
  created_at: string
  expires_at: string
  consumed_at?: string
  claimed_by_ip?: string
  one_time: boolean
  comment: string
  payload: InvitePayload
}

export interface InviteDefaultsResponse {
  server_address: string
  server_port: number
  obfuscation_key: string
  modifier: string
  salt: number
  hub_url: string
}

export interface CreatePresetRequest {
  name: string
  description: string
  rules: RoutingConfig
}

export interface CreateInviteRequest {
  ttl_seconds?: number
  one_time: boolean
  comment: string
  preset?: string
  payload?: InvitePayload
}
