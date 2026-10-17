// DeviceRename.cs —— 设备级改名（DEVPKEY_Device_FriendlyName）实验
//
// 动机：端点名 = `{a45c254e-...},2`（驱动 pin 名，如「扬声器」）+ `(` + `{b3f8fa53-...},6`
//        （设备接口名，对 root 枚举设备来自设备自身 FriendlyName）+ `)`。
//       端点自己的属性存储写不进去（E_ACCESSDENIED，见 EndpointRename.cs 实测），
//       但**底层设备**的 FriendlyName 是 SetupDi 可以改的（设备管理器/蓝牙设备改名同源）。
// 本文件：枚举 root 设备，用 SetupDiSetDevicePropertyW 改 DEVPKEY_Device_FriendlyName，
//        并读回验证。可用 -Revert 恢复原值（原值会被打印出来）。
//
// 用法（PowerShell，需管理员）：
//   Add-Type -Path DeviceRename.cs
//   [MpvDev.DeviceTool]::List()                       # 列出 root 设备
//   [MpvDev.DeviceTool]::Find('steamstreamingspeakers')
//   [MpvDev.DeviceTool]::GetFriendly('<instanceId>')
//   [MpvDev.DeviceTool]::SetFriendly('<instanceId>', '妙播设备')
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace MpvDev
{
    public static class DeviceTool
    {
        const uint DIGCF_PRESENT = 0x02;
        const uint DIGCF_ALLCLASSES = 0x04;
        const int SPDRP_FRIENDLYNAME = 0x0000000C;
        const int SPDRP_DEVICEDESC = 0x00000000;
        const uint DIF_PROPERTYCHANGE = 0x12;
        const uint DICS_ENABLE = 0x01, DICS_DISABLE = 0x02;
        const uint DICS_FLAG_GLOBAL = 0x01;

        [StructLayout(LayoutKind.Sequential)]
        struct SP_DEVINFO_DATA
        {
            public uint cbSize;
            public Guid ClassGuid;
            public uint DevInst;
            public IntPtr Reserved;
            public static SP_DEVINFO_DATA New()
            {
                var d = new SP_DEVINFO_DATA();
                d.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVINFO_DATA));
                return d;
            }
        }

        [StructLayout(LayoutKind.Sequential)]
        struct SP_CLASSINSTALL_HEADER { public uint cbSize; public uint InstallFunction; }

        [StructLayout(LayoutKind.Sequential)]
        struct SP_PROPCHANGE_PARAMS
        {
            public SP_CLASSINSTALL_HEADER ClassInstallHeader;
            public uint StateChange;
            public uint Scope;
            public uint HwProfile;
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
            [FieldOffset(8)] public uint ulVal;
        }

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr SetupDiGetClassDevsW(IntPtr ClassGuid, string Enumerator, IntPtr hwndParent, uint Flags);

        [DllImport("setupapi.dll", SetLastError = true)]
        static extern bool SetupDiEnumDeviceInfo(IntPtr DeviceInfoSet, uint MemberIndex, ref SP_DEVINFO_DATA DeviceInfoData);

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiGetDeviceInstanceIdW(IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData,
            StringBuilder DeviceInstanceId, uint DeviceInstanceIdSize, out uint RequiredSize);

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiGetDeviceRegistryPropertyW(IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData,
            uint Property, out uint PropertyRegDataType, byte[] PropertyBuffer, uint PropertyBufferSize, out uint RequiredSize);

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiSetDevicePropertyW(IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData,
            ref DEVPROPKEY PropertyKey, ref DEVPROPVARIANT PropertyValue, uint Flags);

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiGetDevicePropertyW(IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData,
            ref DEVPROPKEY PropertyKey, out uint PropertyType, byte[] PropertyBuffer,
            uint PropertyBufferSize, out uint RequiredSize, uint Flags);

        [DllImport("setupapi.dll", SetLastError = true)]
        static extern bool SetupDiDestroyDeviceInfoList(IntPtr DeviceInfoSet);

        [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool SetupDiSetClassInstallParamsW(IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData,
            ref SP_PROPCHANGE_PARAMS ClassInstallParams, uint ClassInstallParamsSize);

        [DllImport("setupapi.dll", SetLastError = true)]
        static extern bool SetupDiCallClassInstaller(uint InstallFunction, IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData);

        static readonly DEVPROPKEY PKEY_Device_FriendlyName =
            new DEVPROPKEY { fmtid = new Guid("a45c254e-df1c-4efd-8020-67d146a850e0"), pid = 14 };

        static string RegProp(IntPtr set, ref SP_DEVINFO_DATA d, int prop)
        {
            uint type, need;
            var buf = new byte[1024];
            if (!SetupDiGetDeviceRegistryPropertyW(set, ref d, (uint)prop, out type, buf, (uint)buf.Length, out need))
                return null;
            return Encoding.Unicode.GetString(buf, 0, (int)Math.Min(need, (uint)buf.Length)).TrimEnd('\0');
        }

        /// <summary>枚举 root 枚举器下的所有设备：instanceId | DeviceDesc | FriendlyName</summary>
        public static string[] List()
        {
            var outp = new List<string>();
            IntPtr set = SetupDiGetClassDevsW(IntPtr.Zero, "ROOT", IntPtr.Zero, DIGCF_PRESENT | DIGCF_ALLCLASSES);
            if (set == IntPtr.Zero || set == new IntPtr(-1)) { outp.Add("SetupDiGetClassDevs failed " + Marshal.GetLastWin32Error()); return outp.ToArray(); }
            try
            {
                for (uint i = 0; ; i++)
                {
                    var d = SP_DEVINFO_DATA.New();
                    if (!SetupDiEnumDeviceInfo(set, i, ref d)) break;
                    var sb = new StringBuilder(512); uint need;
                    SetupDiGetDeviceInstanceIdW(set, ref d, sb, 512, out need);
                    string fn = GetFriendly(set, ref d);
                    outp.Add(sb.ToString() + "\t" + RegProp(set, ref d, SPDRP_DEVICEDESC) + "\t" + (fn ?? "<no FriendlyName>"));
                }
            }
            finally { SetupDiDestroyDeviceInfoList(set); }
            return outp.ToArray();
        }

        static string GetFriendly(IntPtr set, ref SP_DEVINFO_DATA d)
        {
            uint type, need;
            var buf = new byte[1024];
            var key = PKEY_Device_FriendlyName;
            if (SetupDiGetDevicePropertyW(set, ref d, ref key, out type, buf, (uint)buf.Length, out need, 0))
                return Encoding.Unicode.GetString(buf, 0, (int)Math.Min(need, (uint)buf.Length)).TrimEnd('\0');
            return RegProp(set, ref d, SPDRP_FRIENDLYNAME);
        }

        /// <summary>按实例 ID 子串找设备，返回 "instanceId | desc | friendly" 多行 + 是否找到</summary>
        public static string Find(string substring)
        {
            var hits = new List<string>();
            foreach (var line in List())
                if (line.IndexOf(substring, StringComparison.OrdinalIgnoreCase) >= 0) hits.Add(line);
            if (hits.Count == 0) return "<not found: " + substring + ">";
            return string.Join("\n", hits.ToArray());
        }

        public static string GetFriendlyById(string instanceId)
        {
            IntPtr set = SetupDiGetClassDevsW(IntPtr.Zero, "ROOT", IntPtr.Zero, DIGCF_PRESENT | DIGCF_ALLCLASSES);
            try
            {
                for (uint i = 0; ; i++)
                {
                    var d = SP_DEVINFO_DATA.New();
                    if (!SetupDiEnumDeviceInfo(set, i, ref d)) break;
                    var sb = new StringBuilder(512); uint need;
                    SetupDiGetDeviceInstanceIdW(set, ref d, sb, 512, out need);
                    if (string.Equals(sb.ToString(), instanceId, StringComparison.OrdinalIgnoreCase))
                    {
                        string f = GetFriendly(set, ref d);
                        return f == null ? "<no FriendlyName>" : f;
                    }
                }
            }
            finally { SetupDiDestroyDeviceInfoList(set); }
            return "<device not found>";
        }

        /// <summary>改设备 FriendlyName；返回逐步骤结果（含 GetLastError）</summary>
        public static string SetFriendly(string instanceId, string newName)
        {
            var log = new List<string>();
            log.Add("BEFORE(friendly) = " + GetFriendlyById(instanceId));
            IntPtr set = SetupDiGetClassDevsW(IntPtr.Zero, "ROOT", IntPtr.Zero, DIGCF_PRESENT | DIGCF_ALLCLASSES);
            try
            {
                for (uint i = 0; ; i++)
                {
                    var d = SP_DEVINFO_DATA.New();
                    if (!SetupDiEnumDeviceInfo(set, i, ref d)) break;
                    var sb = new StringBuilder(512); uint need;
                    SetupDiGetDeviceInstanceIdW(set, ref d, sb, 512, out need);
                    if (!string.Equals(sb.ToString(), instanceId, StringComparison.OrdinalIgnoreCase)) continue;

                    var pv = new DEVPROPVARIANT();
                    pv.vt = 31; // VT_LPWSTR
                    pv.p = Marshal.StringToCoTaskMemUni(newName);
                    var key = PKEY_Device_FriendlyName;
                    bool ok = SetupDiSetDevicePropertyW(set, ref d, ref key, ref pv, 0);
                    log.Add("SetupDiSetDevicePropertyW = " + ok + " lastErr=" + Marshal.GetLastWin32Error());
                    Marshal.FreeCoTaskMem(pv.p);
                    log.Add("AFTER (same set) = " + GetFriendly(set, ref d));
                    log.Add("AFTER (fresh)    = " + GetFriendlyById(instanceId));
                    return string.Join("\n", log.ToArray());
                }
                log.Add("device not found in ROOT set");
            }
            finally { SetupDiDestroyDeviceInfoList(set); }
            return string.Join("\n", log.ToArray());
        }

        /// <summary>重启（disable→enable）指定设备，让改名/属性变化生效</summary>
        public static string Restart(string instanceId)
        {
            var log = new List<string>();
            IntPtr set = SetupDiGetClassDevsW(IntPtr.Zero, "ROOT", IntPtr.Zero, DIGCF_PRESENT | DIGCF_ALLCLASSES);
            try
            {
                for (uint i = 0; ; i++)
                {
                    var d = SP_DEVINFO_DATA.New();
                    if (!SetupDiEnumDeviceInfo(set, i, ref d)) break;
                    var sb = new StringBuilder(512); uint need;
                    SetupDiGetDeviceInstanceIdW(set, ref d, sb, 512, out need);
                    if (!string.Equals(sb.ToString(), instanceId, StringComparison.OrdinalIgnoreCase)) continue;
                    foreach (uint state in new uint[] { DICS_DISABLE, DICS_ENABLE })
                    {
                        var p = new SP_PROPCHANGE_PARAMS();
                        p.ClassInstallHeader.cbSize = (uint)Marshal.SizeOf(typeof(SP_CLASSINSTALL_HEADER));
                        p.ClassInstallHeader.InstallFunction = DIF_PROPERTYCHANGE;
                        p.StateChange = state;
                        p.Scope = DICS_FLAG_GLOBAL;
                        bool a = SetupDiSetClassInstallParamsW(set, ref d, ref p, (uint)Marshal.SizeOf(typeof(SP_PROPCHANGE_PARAMS)));
                        bool b = SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, ref d);
                        log.Add("state=" + state + " setParams=" + a + " callInstaller=" + b
                                + " lastErr=" + Marshal.GetLastWin32Error());
                    }
                    return string.Join("\n", log.ToArray());
                }
                log.Add("device not found");
            }
            finally { SetupDiDestroyDeviceInfoList(set); }
            return string.Join("\n", log.ToArray());
        }
    }
}
