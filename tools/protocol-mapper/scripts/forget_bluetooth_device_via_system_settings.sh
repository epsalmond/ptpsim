#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/forget_bluetooth_device_via_system_settings.sh [options]

Options:
  --name NAME       Bluetooth device name to forget. Default: GFX100 II
  -h, --help        Show this help.

Uses macOS UI automation to click:

  System Settings -> Bluetooth -> <device detail> -> Forget This Device -> Forget Device

This is a fallback for Bluetooth Settings "My Devices" rows that are not
exposed by `blueutil --paired`. It requires Accessibility permission for the
terminal app running this script.
USAGE
}

device_name="${FUJI_CAMERA_NAME:-GFX100 II}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --name)
      device_name="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

echo "+ open x-apple.systempreferences:com.apple.BluetoothSettings" >&2
open "x-apple.systempreferences:com.apple.BluetoothSettings"

osascript - "$device_name" <<'APPLESCRIPT'
on waitForWindow(appName, timeoutSeconds)
  tell application "System Events"
    set deadline to (current date) + timeoutSeconds
    repeat until (current date) > deadline
      if exists process appName then
        tell process appName
          if exists window 1 then return window 1
        end tell
      end if
      delay 0.2
    end repeat
  end tell
  error "System Settings Bluetooth window did not appear"
end waitForWindow

on clickFirstButtonContaining(rootElement, labelText)
  tell application "System Events"
    set targetText to my lowerText(labelText)
    set candidates to entire contents of rootElement
    repeat with itemRef in candidates
      try
        if role of itemRef is "AXButton" then
          set candidateText to ""
          try
            set candidateText to candidateText & " " & (name of itemRef as text)
          end try
          try
            set candidateText to candidateText & " " & (description of itemRef as text)
          end try
          try
            set candidateText to candidateText & " " & (value of itemRef as text)
          end try
          if my lowerText(candidateText) contains targetText then
            click itemRef
            return true
          end if
        end if
      end try
    end repeat
  end tell
  return false
end clickFirstButtonContaining

on lowerText(valueText)
  do shell script "/usr/bin/python3 -c 'import sys; print(sys.stdin.read().casefold())'" with input valueText
end lowerText

on deviceVisible(rootElement, deviceName)
  tell application "System Events"
    set targetText to my lowerText(deviceName)
    set candidates to entire contents of rootElement
    repeat with itemRef in candidates
      try
        if my lowerText(name of itemRef as text) contains targetText then return true
      end try
      try
        if my lowerText(value of itemRef as text) contains targetText then return true
      end try
    end repeat
  end tell
  return false
end deviceVisible

set deviceName to item 1 of argv
set appName to "System Settings"

tell application "System Settings" to activate
set bluetoothWindow to waitForWindow(appName, 10)

tell application "System Events"
  tell process appName
    set deadline to (current date) + 10
    repeat until (current date) > deadline
      if deviceVisible(window 1, deviceName) then exit repeat
      delay 0.2
    end repeat
    if not deviceVisible(window 1, deviceName) then error "Device is not visible in Bluetooth Settings: " & deviceName

    if not clickFirstButtonContaining(window 1, "Show Detail") then error "Could not find a Show Detail button"
    delay 0.5
    if not clickFirstButtonContaining(window 1, "Forget This Device") then error "Could not find Forget This Device button"
    delay 0.5
    if not clickFirstButtonContaining(window 1, "Forget Device") then error "Could not find final Forget Device confirmation button"
  end tell
end tell
APPLESCRIPT
