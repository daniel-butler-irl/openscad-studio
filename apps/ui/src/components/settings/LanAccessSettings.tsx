import { useEffect, useRef, useState } from 'react';
import { getPlatform } from '../../platform';
import type { LanAccessStatus } from '../../platform/types';
import { Button, Text } from '../ui';
import { SettingsCard, SettingsCardHeader, SettingsCardSection } from './SettingsPrimitives';

export function LanAccessSettings() {
  const [status, setStatus] = useState<LanAccessStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [feedback, setFeedback] = useState<string | null>(null);
  const changing = useRef(false);
  const revision = useRef(0);

  useEffect(() => {
    let active = true;
    const refresh = async () => {
      if (changing.current) return;
      const requestRevision = revision.current;
      try {
        const next = await getPlatform().getLanAccessStatus();
        if (active && !changing.current && requestRevision === revision.current) {
          setStatus(next);
          if (next.message) setError(next.message);
        }
      } catch (cause) {
        if (active) setError(String(cause));
      }
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), 3000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, []);

  const toggle = async () => {
    changing.current = true;
    revision.current += 1;
    setBusy(true);
    setError(null);
    setFeedback(null);
    try {
      setStatus(await getPlatform().setLanAccess(!status?.running));
    } catch (cause) {
      setError(String(cause));
    } finally {
      changing.current = false;
      setBusy(false);
    }
  };

  const exportCertificate = async () => {
    setError(null);
    try {
      const platform = getPlatform();
      const certificate = await platform.getLanCertificate();
      const path = await platform.fileSaveAs(
        certificate,
        [{ name: 'Certificate', extensions: ['crt'] }],
        'OpenSCAD-Studio-LAN.crt'
      );
      if (path) setFeedback('Certificate saved. AirDrop it to your iPad to install it.');
    } catch (cause) {
      setError(String(cause));
    }
  };

  const copyAddress = async (url: string) => {
    try {
      await navigator.clipboard.writeText(url);
      setFeedback('Address copied.');
    } catch {
      setError('Could not copy the address. Select it and copy it manually.');
    }
  };

  return (
    <div className="flex flex-col gap-5">
      <SettingsCard>
        <SettingsCardHeader
          title="Open Studio on another device"
          description="Use a browser on the same Wi-Fi or Ethernet network."
        />
        <SettingsCardSection className="flex flex-col gap-4">
          <Text variant="caption" color="secondary">
            Your Mac hosts the web app while Studio is open. Each browser has its own projects, AI
            settings and rendering. Files and open windows on this Mac are not shared.
          </Text>
          <div className="flex items-center justify-between gap-3">
            <Text variant="body" role="status">
              {busy
                ? status?.running
                  ? 'Stopping…'
                  : 'Starting…'
                : status?.running
                  ? 'Available on your network'
                  : status
                    ? 'LAN access is off'
                    : 'Checking status…'}
            </Text>
            <Button
              variant={status?.running ? 'secondary' : 'primary'}
              disabled={busy || !status}
              onClick={() => void toggle()}
            >
              {status?.running ? 'Stop LAN access' : 'Start LAN access'}
            </Button>
          </div>
          {status?.running && (
            <div className="flex flex-col gap-3">
              {status.urls.map((url) => (
                <div key={url} className="flex items-center justify-between gap-2">
                  <Text variant="body" as="code" className="break-all select-all">
                    {url}
                  </Text>
                  <Button variant="ghost" size="sm" onClick={() => void copyAddress(url)}>
                    Copy address
                  </Button>
                </div>
              ))}
              <Text variant="caption" color="secondary">
                Keep this Mac awake. If your network changes, stop and start LAN access to refresh
                the address.
              </Text>
            </div>
          )}
        </SettingsCardSection>
      </SettingsCard>
      <div className="flex flex-col gap-3">
        <Text variant="section-heading">First connection from an iPad</Text>
        <Text variant="caption" color="secondary">
          Install this Mac’s certificate once so Safari can securely load Studio and render models.
        </Text>
        <ol
          className="list-decimal pl-5 space-y-2 text-sm"
          style={{ color: 'var(--text-secondary)' }}
        >
          <li>Save the certificate below and AirDrop it to your iPad.</li>
          <li>
            In iPad Settings → General → VPN &amp; Device Management, install the downloaded
            certificate profile.
          </li>
          <li>
            In General → About → Certificate Trust Settings, enable full trust for OpenSCAD Studio
            LAN.
          </li>
          <li>Start LAN access, then open the displayed HTTPS address in Safari.</li>
        </ol>
        <div>
          <Button variant="secondary" onClick={() => void exportCertificate()}>
            Save certificate…
          </Button>
        </div>
        <Text variant="caption" color="secondary">
          Other devices also need to trust this certificate. Only install the certificate you saved
          from your own Mac.
        </Text>
      </div>
      {(error || status?.message) && (
        <Text variant="caption" role="alert" color="error">
          {error || status?.message}
        </Text>
      )}
      {feedback && (
        <Text variant="caption" role="status">
          {feedback}
        </Text>
      )}
    </div>
  );
}
