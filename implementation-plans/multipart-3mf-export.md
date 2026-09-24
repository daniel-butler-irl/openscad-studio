# Multipart 3MF export

## Goal

Preserve separate top-level OpenSCAD solids, such as a bead and its inset letter, as separate objects in a single 3MF so slicers can assign different materials or colors.

## Approach

Enable OpenSCAD's lazy union only for 3MF export in both desktop and web render services. Keep preview rendering and other export formats unchanged. Verify the generated export arguments and inspect a real 3MF when an available OpenSCAD runtime supports it.

## Affected files

- `apps/ui/src/services/nativeRenderService.ts`
- `apps/ui/src/services/renderService.ts`
- `apps/ui/src/services/aiService.ts`
- Render service tests

## Steps

- [x] Inspect the example SCAD and current export paths.
- [x] Enable multipart 3MF export in both render services.
- [x] Add focused tests for the export arguments and behavior.
- [x] Run relevant validation and inspect the result.
- [x] Teach the in-app copilot to create separate top-level solids for printable colors.

Validation: the bundled OpenSCAD 2026.03.16 exported the example `bead.scad` as a 3MF with two objects, two build items, and two triangle material references. Repository formatting, lint, type check, and all 633 unit tests passed.
