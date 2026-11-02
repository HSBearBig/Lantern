export type BrowserStatus = { id: string; name: string };

export type HostStatus = {
  hostId: string;
  name: string;
  online: boolean;
  enabled: boolean;
  session: string | null;
  pairingUrl: string | null;
  pairingExpiresIn: number;
  pairedBrowsers: BrowserStatus[];
  error: string | null;
};

export type Settings = { serverUrl: string; deviceName: string; autostart: boolean };

export type StartRequest = Settings & { allowInsecureLocalhost: boolean };
