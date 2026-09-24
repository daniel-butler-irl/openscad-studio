/** @jest-environment jsdom */
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { jest } from '@jest/globals';
import type { LanAccessStatus } from '../../platform/types';

const off: LanAccessStatus = { running: false, urls: [], message: null };
const on: LanAccessStatus = { running: true, urls: ['https://192.168.1.10:3443'], message: null };
const getStatus = jest.fn<() => Promise<LanAccessStatus>>();
const setAccess = jest.fn<(enabled: boolean) => Promise<LanAccessStatus>>();
const getCertificate = jest.fn<() => Promise<string>>();
const saveAs = jest.fn<(...args: unknown[]) => Promise<string | null>>();
const copy = jest.fn<(text: string) => Promise<void>>();
jest.unstable_mockModule('@/platform', () => ({
  getPlatform: () => ({
    getLanAccessStatus: getStatus,
    setLanAccess: setAccess,
    getLanCertificate: getCertificate,
    fileSaveAs: saveAs,
  }),
}));
const { LanAccessSettings } = await import('../settings/LanAccessSettings');

beforeEach(() => {
  jest.clearAllMocks();
  getStatus.mockResolvedValue(off);
  setAccess.mockImplementation(async (enabled) => (enabled ? on : off));
  getCertificate.mockResolvedValue('PUBLIC CERTIFICATE');
  saveAs.mockResolvedValue('/tmp/studio.crt');
  copy.mockResolvedValue(undefined);
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: copy } });
});

test('starts only when requested, copies the HTTPS address, and stops', async () => {
  render(<LanAccessSettings />);
  await screen.findByText('LAN access is off');
  expect(setAccess).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Start LAN access' }));
  await screen.findByText(on.urls[0]);
  expect(setAccess).toHaveBeenCalledWith(true);
  fireEvent.click(screen.getByRole('button', { name: 'Copy address' }));
  await waitFor(() => expect(copy).toHaveBeenCalledWith(on.urls[0]));
  fireEvent.click(screen.getByRole('button', { name: 'Stop LAN access' }));
  await screen.findByText('LAN access is off');
  expect(screen.queryByText(on.urls[0])).toBeNull();
  expect(setAccess).toHaveBeenLastCalledWith(false);
});

test('reports startup failures and allows retry without showing a live address', async () => {
  setAccess.mockRejectedValueOnce(new Error('Port 3443 is already in use'));
  render(<LanAccessSettings />);
  await screen.findByText('LAN access is off');
  fireEvent.click(screen.getByRole('button', { name: 'Start LAN access' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Port 3443 is already in use');
  expect(screen.queryByText(on.urls[0])).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: 'Start LAN access' }));
  await screen.findByText(on.urls[0]);
  expect(screen.queryByRole('alert')).toBeNull();
});

test('keeps the stop control available if stopping fails', async () => {
  getStatus.mockResolvedValue(on);
  setAccess.mockRejectedValue(new Error('Could not stop'));
  render(<LanAccessSettings />);
  fireEvent.click(await screen.findByRole('button', { name: 'Stop LAN access' }));
  await screen.findByRole('alert');
  expect(screen.getByRole('button', { name: 'Stop LAN access' })).toBeEnabled();
});

test('exports only the public certificate using the native save dialog', async () => {
  render(<LanAccessSettings />);
  fireEvent.click(screen.getByRole('button', { name: 'Save certificate…' }));
  await waitFor(() =>
    expect(saveAs).toHaveBeenCalledWith(
      'PUBLIC CERTIFICATE',
      [{ name: 'Certificate', extensions: ['crt'] }],
      'OpenSCAD-Studio-LAN.crt'
    )
  );
  expect(setAccess).not.toHaveBeenCalled();
  expect(await screen.findByText(/Certificate saved/)).toBeVisible();
});

test('cancelling certificate export does not claim it was saved', async () => {
  saveAs.mockResolvedValue(null);
  render(<LanAccessSettings />);
  fireEvent.click(screen.getByRole('button', { name: 'Save certificate…' }));
  await waitFor(() => expect(saveAs).toHaveBeenCalled());
  expect(screen.queryByText(/Certificate saved/)).toBeNull();
});
