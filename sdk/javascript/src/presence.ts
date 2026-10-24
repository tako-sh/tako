/** One live connection. Multiple tabs belonging to a subject remain separate. */
export interface PresenceMember<T = unknown> {
  connectionId: string;
  /** Identity returned by auth.verify; absent for anonymous connections. */
  subject?: string;
  data: T;
}

/** Server-authored snapshot, separate from durable channel messages. */
export interface PresenceSnapshot<T = unknown> {
  type: "tako.presence";
  connectionId: string;
  members: PresenceMember<T>[];
}

export const PRESENCE_CHANNEL_PREFIX = "tako.presence:";
