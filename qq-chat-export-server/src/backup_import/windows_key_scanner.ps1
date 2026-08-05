# Independently authored from the memory-layout strategy documented by
# QQBackup/x_key_scanner. No source from that unlicensed repository is copied.
$ErrorActionPreference = 'Stop'

$source = @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;

namespace Qce.NtqqKeyScan
{
    internal static class Native
    {
        internal const uint PROCESS_VM_READ = 0x0010;
        internal const uint PROCESS_QUERY_INFORMATION = 0x0400;
        internal const uint MEM_COMMIT = 0x1000;
        internal const uint PAGE_GUARD = 0x100;
        internal const uint PAGE_NOACCESS = 0x01;

        [StructLayout(LayoutKind.Sequential)]
        internal struct MEMORY_BASIC_INFORMATION
        {
            internal IntPtr BaseAddress;
            internal IntPtr AllocationBase;
            internal uint AllocationProtect;
            internal UIntPtr RegionSize;
            internal uint State;
            internal uint Protect;
            internal uint Type;
        }

        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern IntPtr OpenProcess(
            uint desiredAccess,
            bool inheritHandle,
            int processId);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool ReadProcessMemory(
            IntPtr process,
            IntPtr baseAddress,
            byte[] buffer,
            UIntPtr size,
            out UIntPtr bytesRead);

        [DllImport("kernel32.dll", SetLastError = true)]
        internal static extern UIntPtr VirtualQueryEx(
            IntPtr process,
            IntPtr address,
            out MEMORY_BASIC_INFORMATION information,
            UIntPtr informationLength);

        [DllImport("kernel32.dll")]
        [return: MarshalAs(UnmanagedType.Bool)]
        internal static extern bool CloseHandle(IntPtr handle);
    }

    public static class Scanner
    {
        private const int KeyLength = 16;
        private const int Alignment = 16;
        private const int Radius = 0x200;
        private const int ChunkSize = 8 * 1024 * 1024;
        private const int ChunkOverlap = 0x600;
        private const ulong MaxRegionBytes = 512UL * 1024UL * 1024UL;
        private const ulong MaxProcessBytes = 2UL * 1024UL * 1024UL * 1024UL;
        private const int MaxCandidates = 2048;

        private static readonly byte[] Anchor = new byte[] {
            0x09, (byte)'H', (byte)'M', (byte)'A', (byte)'C',
            (byte)'_', (byte)'S', (byte)'H', (byte)'A', (byte)'1'
        };

        public static string[] Scan()
        {
            Process[] qqProcesses = Process.GetProcessesByName("QQ");
            if (qqProcesses.Length == 0)
            {
                return new string[] { "QCE_STATUS:NO_QQ" };
            }

            List<Process> targets = new List<Process>();
            foreach (Process process in qqProcesses)
            {
                try
                {
                    if (HasWrapperNode(process))
                    {
                        targets.Add(process);
                    }
                }
                catch
                {
                    // Module enumeration can fail for protected child processes.
                }
            }
            if (targets.Count == 0)
            {
                return new string[] { "QCE_STATUS:QQ_NOT_READY" };
            }

            List<string> candidates = new List<string>();
            HashSet<string> seen = new HashSet<string>(StringComparer.Ordinal);
            int opened = 0;
            foreach (Process process in targets)
            {
                IntPtr handle = Native.OpenProcess(
                    Native.PROCESS_QUERY_INFORMATION | Native.PROCESS_VM_READ,
                    false,
                    process.Id);
                if (handle == IntPtr.Zero)
                {
                    continue;
                }
                opened++;
                try
                {
                    ScanProcess(handle, candidates, seen);
                    if (candidates.Count >= MaxCandidates)
                    {
                        break;
                    }
                }
                finally
                {
                    Native.CloseHandle(handle);
                }
            }

            if (candidates.Count > 0)
            {
                List<string> output = new List<string>();
                output.Add("QCE_STATUS:OK");
                foreach (string candidate in candidates)
                {
                    output.Add("QCE_KEY:" + candidate);
                }
                return output.ToArray();
            }
            return new string[] {
                opened == 0 ? "QCE_STATUS:ACCESS_DENIED" : "QCE_STATUS:NO_CANDIDATE"
            };
        }

        public static string[] SelfTest()
        {
            byte[] data = new byte[4096];
            int slot = 0x800;
            Array.Copy(Anchor, 0, data, slot + 1, Anchor.Length);
            byte[] key = System.Text.Encoding.ASCII.GetBytes("QceTestKey!2345?");
            Array.Copy(key, 0, data, slot + 0x100, key.Length);
            List<string> candidates = new List<string>();
            HashSet<string> seen = new HashSet<string>(StringComparer.Ordinal);
            ScanBuffer(data, data.Length, 0, candidates, seen);
            List<string> output = new List<string>();
            output.Add("QCE_STATUS:OK");
            foreach (string candidate in candidates)
            {
                output.Add("QCE_KEY:" + candidate);
            }
            return output.ToArray();
        }

        private static bool HasWrapperNode(Process process)
        {
            foreach (ProcessModule module in process.Modules)
            {
                if (string.Equals(module.ModuleName, "wrapper.node", StringComparison.OrdinalIgnoreCase))
                {
                    return true;
                }
            }
            return false;
        }

        private static bool IsReadable(uint protection)
        {
            if ((protection & Native.PAGE_GUARD) != 0 || (protection & Native.PAGE_NOACCESS) != 0)
            {
                return false;
            }
            uint basic = protection & 0xff;
            return basic == 0x02 || basic == 0x04 || basic == 0x08 ||
                   basic == 0x20 || basic == 0x40 || basic == 0x80;
        }

        private static void ScanProcess(
            IntPtr handle,
            List<string> candidates,
            HashSet<string> seen)
        {
            ulong address = 0;
            ulong scanned = 0;
            UIntPtr infoSize = new UIntPtr((uint)Marshal.SizeOf(typeof(Native.MEMORY_BASIC_INFORMATION)));
            while (candidates.Count < MaxCandidates && scanned < MaxProcessBytes)
            {
                Native.MEMORY_BASIC_INFORMATION information;
                UIntPtr result = Native.VirtualQueryEx(
                    handle,
                    new IntPtr(unchecked((long)address)),
                    out information,
                    infoSize);
                if (result == UIntPtr.Zero)
                {
                    break;
                }

                ulong regionBase = unchecked((ulong)information.BaseAddress.ToInt64());
                ulong regionSize = information.RegionSize.ToUInt64();
                ulong next = regionBase + regionSize;
                if (next <= address || regionSize == 0)
                {
                    break;
                }

                if (information.State == Native.MEM_COMMIT && IsReadable(information.Protect))
                {
                    ulong allowed = Math.Min(regionSize, MaxRegionBytes);
                    allowed = Math.Min(allowed, MaxProcessBytes - scanned);
                    ScanRegion(handle, regionBase, allowed, candidates, seen);
                    scanned += allowed;
                }
                address = next;
            }
        }

        private static void ScanRegion(
            IntPtr handle,
            ulong regionBase,
            ulong regionSize,
            List<string> candidates,
            HashSet<string> seen)
        {
            ulong offset = 0;
            while (offset < regionSize && candidates.Count < MaxCandidates)
            {
                int requested = (int)Math.Min((ulong)ChunkSize, regionSize - offset);
                byte[] buffer = new byte[requested];
                UIntPtr bytesRead;
                bool ok = Native.ReadProcessMemory(
                    handle,
                    new IntPtr(unchecked((long)(regionBase + offset))),
                    buffer,
                    new UIntPtr((uint)requested),
                    out bytesRead);
                int count = (int)Math.Min((ulong)requested, bytesRead.ToUInt64());
                if (ok || count > 0)
                {
                    ScanBuffer(buffer, count, regionBase + offset, candidates, seen);
                }
                if (requested <= ChunkOverlap)
                {
                    break;
                }
                offset += (ulong)(requested - ChunkOverlap);
            }
        }

        private static void ScanBuffer(
            byte[] buffer,
            int count,
            ulong bufferAddress,
            List<string> candidates,
            HashSet<string> seen)
        {
            int search = 0;
            while (search + Anchor.Length <= count && candidates.Count < MaxCandidates)
            {
                int found = Array.IndexOf(buffer, Anchor[0], search, count - search);
                if (found < 0 || found + Anchor.Length > count)
                {
                    break;
                }
                bool matches = true;
                for (int index = 1; index < Anchor.Length; index++)
                {
                    if (buffer[found + index] != Anchor[index])
                    {
                        matches = false;
                        break;
                    }
                }
                if (matches && found > 0)
                {
                    ulong slotAddress = bufferAddress + (ulong)(found - 1);
                    if ((slotAddress & (Alignment - 1)) == 0)
                    {
                        CollectNear(buffer, count, bufferAddress, slotAddress, candidates, seen);
                    }
                }
                search = found + 1;
            }
        }

        private static void CollectNear(
            byte[] buffer,
            int count,
            ulong bufferAddress,
            ulong center,
            List<string> candidates,
            HashSet<string> seen)
        {
            for (int distance = 0; distance <= Radius && candidates.Count < MaxCandidates; distance += Alignment)
            {
                TryCandidate(buffer, count, bufferAddress, center - (ulong)distance, candidates, seen);
                if (distance != 0)
                {
                    TryCandidate(buffer, count, bufferAddress, center + (ulong)distance, candidates, seen);
                }
            }
        }

        private static void TryCandidate(
            byte[] buffer,
            int count,
            ulong bufferAddress,
            ulong candidateAddress,
            List<string> candidates,
            HashSet<string> seen)
        {
            if (candidateAddress < bufferAddress)
            {
                return;
            }
            ulong relative = candidateAddress - bufferAddress;
            if (relative > (ulong)Int32.MaxValue)
            {
                return;
            }
            int offset = (int)relative;
            if (offset < 0 || offset + KeyLength > count)
            {
                return;
            }

            bool hasSymbol = false;
            for (int index = 0; index < KeyLength; index++)
            {
                byte value = buffer[offset + index];
                if (value < 0x21 || value > 0x7e)
                {
                    return;
                }
                bool alphanumeric = (value >= (byte)'0' && value <= (byte)'9') ||
                                    (value >= (byte)'A' && value <= (byte)'Z') ||
                                    (value >= (byte)'a' && value <= (byte)'z');
                if (!alphanumeric)
                {
                    hasSymbol = true;
                }
            }
            if (!hasSymbol)
            {
                return;
            }

            char[] hex = new char[KeyLength * 2];
            const string digits = "0123456789abcdef";
            for (int index = 0; index < KeyLength; index++)
            {
                byte value = buffer[offset + index];
                hex[index * 2] = digits[value >> 4];
                hex[index * 2 + 1] = digits[value & 0x0f];
            }
            string encoded = new string(hex);
            if (seen.Add(encoded))
            {
                candidates.Add(encoded);
            }
        }
    }
}
'@

try {
    Add-Type -TypeDefinition $source -Language CSharp
    if ($env:QCE_KEY_SCANNER_SELF_TEST -eq '1') {
        [Qce.NtqqKeyScan.Scanner]::SelfTest()
    }
    else {
        [Qce.NtqqKeyScan.Scanner]::Scan()
    }
}
catch {
    Write-Output 'QCE_STATUS:ERROR'
    exit 1
}
