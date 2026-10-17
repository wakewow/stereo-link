// DevicePropDiag.cs —— 设备级改名的严谨判定（正对照 + 清零 last-error + 两套 API）
//
// 为什么要这个文件：DeviceRename.cs 里 SetupDiSetDevicePropertyW 返回 False / lastErr=1004，
// 但 1004(ERROR_INVALID_FLAGS) 既可能是「API 拒绝」也可能是「P/Invoke 结构体/编组有误 + 陈旧
// last-error」。判定「被硬拒」之前必须先证明**同一套结构体能不能成功读**一个已知存在的设备属性。
//
// 本文件做四件事（每步先 SetLastError(0)，避免陈旧错误码）：
//   1) 正对照：SetupDiGetDevicePropertyW 读 DEVPKEY_Device_DeviceDesc(pid=2) —— 已知有值
//   2) 目标：   SetupDiGetDevicePropertyW 读 DEVPKEY_Device_FriendlyName(pid=14) —— 预期不存在
//   3) 写 1：   SetupDiSetDevicePropertyW(DEVPKEY_Device_FriendlyName, VT_LPWSTR)
//   4) 写 2：   SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME=0x0C)  ← 传统「设备改名」API
//               （写的是设备硬件键下的 FriendlyName 值；devcon/设备管理器同源）
// 用法：
//   Add-Type -Path DevicePropDiag.cs
//   [MpvDiag.DiagTool]::Run('ROOT\STEAMSTREAMINGSPEAKERS\0000', '小米妙播设备', $true)
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace MpvDiag
{
    public static class DiagTool
    {
        const uint DIGCF_PRESENT = 0x02, DIGCF_ALLCLASSES = 0x04;
        const uint SPDRP_DEVICEDESC = 0x00, SPDRP_FRIENDLYNAME = 0x0C;
        const ushort VT_LPWSTR = 31;

        [DllImport("kernel32.dll", SetLastError = false)]
        static extern void SetLastError(uint dwErrCode);

        [StructLayout(LayoutKind.Sequential)]
        struct SP_DEVINFO_DATA
        {
            public uint cbSize; public Guid ClassGuid; public uint DevInst; public IntPtr Reserved;
            public static SP_DEVINFO_DATA New() { var d = new SP_DEVINFO_DATA(); d.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVINFO_DATA)); return d; }
        }

        [StructLayout(LayoutKind.Sequential, Pack = 4)]
        struct DEVPROPKEY { public Guid fmtid; public uint pid; }

        [StructLayout(LayoutKind.Explicit)]
        struct DEVPROPVARIANT
        {
            [FieldOffset(0)] public ushort vt;
            [FieldOffset(2)] public ushort r1;
            [FieldOffset(4)] public ushort r2;
            [FieldOffset(6)] public ushort r3;
            [FieldOffset(8)] public IntPtr p;
            [FieldOffset(8)] public ulong u;
        }

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr SetupDiGetClassDevsW(IntPtr ClassGuid, string Enumerator, IntPtr hwndParent, uint Flags);
        [DllImport("setupapi.dll", SetLastError = true)]
        static extern bool SetupDiEnumDeviceInfo(IntPtr set, uint idx, ref SP_DEVINFO_DATA d);
        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiGetDeviceInstanceIdW(IntPtr set, ref SP_DEVINFO_DATA d, StringBuilder id, uint size, out uint need);
        [DllImport("setupapi.dll", SetLastError = true)]
        static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiGetDevicePropertyW(IntPtr set, ref SP_DEVINFO_DATA d, ref DEVPROPKEY key,
            out uint type, byte[] buf, uint bufSize, out uint need, uint flags);
        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiSetDevicePropertyW(IntPtr set, ref SP_DEVINFO_DATA d, ref DEVPROPKEY key,
            ref DEVPROPVARIANT val, uint flags);
        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiGetDeviceRegistryPropertyW(IntPtr set, ref SP_DEVINFO_DATA d, uint prop,
            out uint type, byte[] buf, uint bufSize, out uint need);
        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr set, ref SP_DEVINFO_DATA d, uint prop,
            byte[] buf, uint bufSize);

        const string FMT_DEVICE = "a45c254e-df1c-4efd-8020-67d146a850e0"; // DEVPKEY_Device_*

        static string PropStr(IntPtr set, ref SP_DEVINFO_DATA d, uint pid)
        {
            var key = new DEVPROPKEY { fmtid = new Guid(FMT_DEVICE), pid = pid };
            uint type, need;
            var buf = new byte[2048];
            SetLastError(0);
            bool ok = SetupDiGetDevicePropertyW(set, ref d, ref key, out type, buf, (uint)buf.Length, out need, 0);
            int err = Marshal.GetLastWin32Error();
            if (!ok) return string.Format("FAIL ok={0} type={1} need={2} lastErr={3}", ok, type, need, err);
            return string.Format("OK type={0} len={1} val='{2}'", type, need,
                Encoding.Unicode.GetString(buf, 0, (int)Math.Min(need, (uint)buf.Length)).TrimEnd('\0'));
        }

        static string RegPropStr(IntPtr set, ref SP_DEVINFO_DATA d, uint prop)
        {
            uint type, need;
            var buf = new byte[2048];
            SetLastError(0);
            bool ok = SetupDiGetDeviceRegistryPropertyW(set, ref d, prop, out type, buf, (uint)buf.Length, out need);
            int err = Marshal.GetLastWin32Error();
            if (!ok) return string.Format("FAIL lastErr={0}", err);
            return string.Format("OK type={0} val='{1}'", type,
                Encoding.Unicode.GetString(buf, 0, (int)Math.Min(need, (uint)buf.Length)).TrimEnd('\0'));
        }

        public static string Run(string instanceId, string newName, bool doWrite)
        {
            var log = new List<string>();
            log.Add("instanceId = " + instanceId + "  newName='" + newName + "'  doWrite=" + doWrite);
            IntPtr set = SetupDiGetClassDevsW(IntPtr.Zero, "ROOT", IntPtr.Zero, DIGCF_PRESENT | DIGCF_ALLCLASSES);
            if (set == IntPtr.Zero || set == new IntPtr(-1)) { log.Add("GetClassDevs FAIL lastErr=" + Marshal.GetLastWin32Error()); return string.Join("\n", log.ToArray()); }
            try
            {
                bool found = false;
                for (uint i = 0; !found; i++)
                {
                    var d = SP_DEVINFO_DATA.New();
                    SetLastError(0);
                    if (!SetupDiEnumDeviceInfo(set, i, ref d)) { log.Add("device not found in ROOT set (enum ended at " + i + ")"); break; }
                    var sb = new StringBuilder(512); uint need;
                    SetupDiGetDeviceInstanceIdW(set, ref d, sb, 512, out need);
                    if (!string.Equals(sb.ToString(), instanceId, StringComparison.OrdinalIgnoreCase)) continue;
                    found = true;

                    log.Add("[1] positive control: DEVPKEY_Device_DeviceDesc(pid=2) via SetupDiGetDevicePropertyW -> " + PropStr(set, ref d, 2));
                    log.Add("[2] target          : DEVPKEY_Device_FriendlyName(pid=14) via SetupDiGetDevicePropertyW -> " + PropStr(set, ref d, 14));
                    log.Add("[2b] legacy get     : SPDRP_FRIENDLYNAME via SetupDiGetDeviceRegistryPropertyW -> " + RegPropStr(set, ref d, SPDRP_FRIENDLYNAME));

                    if (!doWrite) return string.Join("\n", log.ToArray());

                    // 写 1：SetupDiSetDevicePropertyW
                    var key = new DEVPROPKEY { fmtid = new Guid(FMT_DEVICE), pid = 14 };
                    var pv = new DEVPROPVARIANT(); pv.vt = VT_LPWSTR; pv.p = Marshal.StringToCoTaskMemUni(newName);
                    SetLastError(0);
                    bool ok1 = SetupDiSetDevicePropertyW(set, ref d, ref key, ref pv, 0);
                    int err1 = Marshal.GetLastWin32Error();
                    Marshal.FreeCoTaskMem(pv.p);
                    log.Add("[3] WRITE SetupDiSetDevicePropertyW(FriendlyName) ok=" + ok1 + " lastErr=" + err1);
                    log.Add("[3b] read back -> " + PropStr(set, ref d, 14));
                    log.Add("[3c] DeviceDesc after write -> " + PropStr(set, ref d, 2));

                    // 写 2：SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)
                    byte[] raw = Encoding.Unicode.GetBytes(newName + "\0");
                    SetLastError(0);
                    bool ok2 = SetupDiSetDeviceRegistryPropertyW(set, ref d, SPDRP_FRIENDLYNAME, raw, (uint)raw.Length);
                    int err2 = Marshal.GetLastWin32Error();
                    log.Add("[4] WRITE SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME) ok=" + ok2 + " lastErr=" + err2);
                    log.Add("[4b] legacy read back -> " + RegPropStr(set, ref d, SPDRP_FRIENDLYNAME));
                    log.Add("[4c] DEVPKEY read back -> " + PropStr(set, ref d, 14));
                    return string.Join("\n", log.ToArray());
                }
            }
            finally { SetupDiDestroyDeviceInfoList(set); }
            return string.Join("\n", log.ToArray());
        }
    }
}
