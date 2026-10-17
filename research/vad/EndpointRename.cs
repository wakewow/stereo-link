// EndpointRename.cs —— 用 Windows 官方 MMDevice API 读/写音频端点属性
//
// 目的：验证「不改驱动能否改掉 Windows 声音输出列表里的端点名」。
// 两条路：
//   A) 直接写注册表 HKLM\...\MMDevices\Audio\Render\{guid}\Properties  -> 实测被 ACL 拒绝
//   B) IMMDeviceEnumerator -> IMMDevice::OpenPropertyStore(STGM_READWRITE)
//      -> IPropertyStore::SetValue(PKEY_Device_FriendlyName) -> Commit()
//      （这是 Windows「设置 → 声音 → 设备属性 → 重命名」走的接口，写操作由 audiosrv(SYSTEM) 代执行）
// 本文件实现 B，并把每个 HRESULT 原样返回，便于记录失败原因。
//
// 编译/使用见 research/vad/rename-probe.md：
//   Add-Type -Path EndpointRename.cs
//   [MpvRename.EndpointTool]::ListRender()
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

namespace MpvRename
{
    // ---------------------------------------------------------------- 常量
    internal static class Guids
    {
        public const string CLSID_MMDeviceEnumerator = "BCDE0395-E52F-467C-8E3D-C4579291692E";
        public const string IID_IMMDeviceEnumerator  = "A95664D2-9614-4F35-A746-DE8DB63617E6";
        public const string IID_IMMDevice            = "D666063F-1587-4E43-81F1-B948E807363F";
        public const string IID_IMMDeviceCollection  = "0BD7A1BE-7A1A-44DB-8397-CC5392387B5E";
        public const string IID_IPropertyStore       = "886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99";
    }

    internal enum EDataFlow { eRender = 0, eCapture = 1, eAll = 2 }

    [Flags]
    internal enum DeviceState { ACTIVE = 0x1, DISABLED = 0x2, NOTPRESENT = 0x4, UNPLUGGED = 0x8, ALL = 0xF }

    // STGM
    internal static class Stgm { public const uint READ = 0, WRITE = 1, READWRITE = 2; }

    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    internal struct PROPERTYKEY
    {
        public Guid fmtid;
        public uint pid;
        public PROPERTYKEY(string g, uint p) { fmtid = new Guid(g); pid = p; }
    }

    // PROPVARIANT：只支持我们要用的几种（LPWSTR / UI4）；其余靠 PropVariantClear 释放
    [StructLayout(LayoutKind.Explicit)]
    internal struct PROPVARIANT
    {
        [FieldOffset(0)] public ushort vt;
        [FieldOffset(2)] public ushort wReserved1;
        [FieldOffset(4)] public ushort wReserved2;
        [FieldOffset(6)] public ushort wReserved3;
        [FieldOffset(8)] public IntPtr p;
        [FieldOffset(8)] public uint ulVal;
        [FieldOffset(8)] public int lVal;
    }

    [ComImport, Guid(Guids.IID_IPropertyStore), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IPropertyStore
    {
        [PreserveSig] int GetCount(out uint cProps);
        [PreserveSig] int GetAt(uint iProp, out PROPERTYKEY pkey);
        [PreserveSig] int GetValue(ref PROPERTYKEY key, out PROPVARIANT pv);
        [PreserveSig] int SetValue(ref PROPERTYKEY key, ref PROPVARIANT pv);
        [PreserveSig] int Commit();
    }

    [ComImport, Guid(Guids.IID_IMMDevice), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMDevice
    {
        [PreserveSig] int Activate(ref Guid iid, uint dwClsCtx, IntPtr pActivationParams,
                                   [MarshalAs(UnmanagedType.IUnknown)] out object ppInterface);
        [PreserveSig] int OpenPropertyStore(uint stgmAccess, out IPropertyStore ppProperties);
        [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string ppstrId);
        [PreserveSig] int GetState(out uint pdwState);
    }

    [ComImport, Guid(Guids.IID_IMMDeviceCollection), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMDeviceCollection
    {
        [PreserveSig] int GetCount(out uint pcDevices);
        [PreserveSig] int Item(uint nDevice, out IMMDevice ppDevice);
    }

    [ComImport, Guid(Guids.IID_IMMDeviceEnumerator), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMDeviceEnumerator
    {
        [PreserveSig] int EnumAudioEndpoints(EDataFlow dataFlow, DeviceState dwStateMask,
                                            out IMMDeviceCollection ppDevices);
        [PreserveSig] int GetDefaultAudioEndpoint(EDataFlow dataFlow, int role, out IMMDevice ppEndpoint);
        [PreserveSig] int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string pwstrId, out IMMDevice ppDevice);
        [PreserveSig] int RegisterEndpointNotificationCallback(IntPtr pClient);
        [PreserveSig] int UnregisterEndpointNotificationCallback(IntPtr pClient);
    }

    public static class EndpointTool
    {
        internal static readonly PROPERTYKEY PKEY_Device_FriendlyName =
            new PROPERTYKEY("a45c254e-df1c-4efd-8020-67d146a850e0", 14);
        internal static readonly PROPERTYKEY PKEY_Device_DeviceDesc =
            new PROPERTYKEY("a45c254e-df1c-4efd-8020-67d146a850e0", 2);
        internal static readonly PROPERTYKEY PKEY_DeviceInterface_FriendlyName =
            new PROPERTYKEY("b3f8fa53-0004-438e-9003-51a46e139bfc", 6);

        [DllImport("ole32.dll")] internal static extern void PropVariantClear(ref PROPVARIANT pv);
        [DllImport("ole32.dll")] internal static extern int CoCreateInstance(ref Guid clsid, IntPtr outer,
            uint clsctx, ref Guid iid, [MarshalAs(UnmanagedType.IUnknown)] out object ppv);
        [DllImport("ole32.dll")] internal static extern int CoInitializeEx(IntPtr pvReserved, uint dwCoInit);

        internal static IMMDeviceEnumerator NewEnumerator()
        {
            CoInitializeEx(IntPtr.Zero, 0x2 /*APARTMENTTHREADED*/);
            Guid clsid = new Guid(Guids.CLSID_MMDeviceEnumerator);
            Guid iid = new Guid(Guids.IID_IMMDeviceEnumerator);
            object o;
            int hr = CoCreateInstance(ref clsid, IntPtr.Zero, 0x17 /*CLSCTX_ALL*/, ref iid, out o);
            if (hr != 0) throw new COMException("CoCreateInstance(MMDeviceEnumerator) failed", hr);
            return (IMMDeviceEnumerator)o;
        }

        internal static string ReadString(IPropertyStore ps, PROPERTYKEY key)
        {
            PROPVARIANT pv;
            int hr = ps.GetValue(ref key, out pv);
            if (hr != 0) return "<GetValue hr=0x" + hr.ToString("X8") + ">";
            try
            {
                if (pv.vt == 31 /*VT_LPWSTR*/) return Marshal.PtrToStringUni(pv.p);
                return "<vt=" + pv.vt + ">";
            }
            finally { PropVariantClear(ref pv); }
        }

        /// <summary>列出某方向端点的 Id / 状态 / 三个名字。</summary>
        public static string[] List(int flow /*0=render,1=capture*/, bool activeOnly)
        {
            var list = new List<string>();
            IMMDeviceEnumerator en = NewEnumerator();
            IMMDeviceCollection col;
            int hr = en.EnumAudioEndpoints((EDataFlow)flow,
                activeOnly ? DeviceState.ACTIVE : DeviceState.ALL, out col);
            if (hr != 0) { list.Add("EnumAudioEndpoints hr=0x" + hr.ToString("X8")); return list.ToArray(); }
            uint n; col.GetCount(out n);
            for (uint i = 0; i < n; i++)
            {
                IMMDevice dev; col.Item(i, out dev);
                string id; dev.GetId(out id);
                uint st; dev.GetState(out st);
                IPropertyStore ps;
                int hrOpen = dev.OpenPropertyStore(Stgm.READ, out ps);
                string fn, dd, ifn;
                if (hrOpen == 0)
                {
                    fn = ReadString(ps, PKEY_Device_FriendlyName);
                    dd = ReadString(ps, PKEY_Device_DeviceDesc);
                    ifn = ReadString(ps, PKEY_DeviceInterface_FriendlyName);
                }
                else { fn = dd = ifn = "<OpenPropertyStore hr=0x" + hrOpen.ToString("X8") + ">"; }
                list.Add(id + "\t" + st + "\t" + fn + "\t" + dd + "\t" + ifn);
            }
            return list.ToArray();
        }

        /// <summary>按端点 Id 读 FriendlyName（走官方 API，不走注册表）。</summary>
        public static string GetFriendlyName(string deviceId)
        {
            IMMDeviceEnumerator en = NewEnumerator();
            IMMDevice dev;
            int hr = en.GetDevice(deviceId, out dev);
            if (hr != 0) return "<GetDevice hr=0x" + hr.ToString("X8") + ">";
            IPropertyStore ps;
            int hr2 = dev.OpenPropertyStore(Stgm.READ, out ps);
            if (hr2 != 0) return "<OpenPropertyStore(READ) hr=0x" + hr2.ToString("X8") + ">";
            return ReadString(ps, PKEY_Device_FriendlyName);
        }

        /// <summary>
        /// 核心实验：OpenPropertyStore(STGM_READWRITE) -> SetValue(FriendlyName) -> Commit。
        /// 返回多行文本，逐步骤带 HRESULT。
        /// </summary>
        public static string TrySetFriendlyName(string deviceId, string newName)
        {
            var log = new List<string>();
            IMMDeviceEnumerator en = NewEnumerator();
            IMMDevice dev;
            int hr = en.GetDevice(deviceId, out dev);
            log.Add("GetDevice            hr=0x" + hr.ToString("X8"));
            if (hr != 0) return string.Join("\n", log.ToArray());

            foreach (uint stgm in new uint[] { Stgm.READWRITE, Stgm.WRITE })
            {
                IPropertyStore ps;
                int h = dev.OpenPropertyStore(stgm, out ps);
                log.Add("OpenPropertyStore(stgm=" + stgm + ") hr=0x" + h.ToString("X8")
                        + (h == 0 ? " OK" : " FAIL"));
                if (h != 0) continue;

                PROPERTYKEY key = PKEY_Device_FriendlyName;
                var pv = new PROPVARIANT();
                pv.vt = 31; // VT_LPWSTR
                pv.p = Marshal.StringToCoTaskMemUni(newName);
                int hSet = ps.SetValue(ref key, ref pv);
                log.Add("SetValue(FriendlyName) hr=0x" + hSet.ToString("X8") + (hSet == 0 ? " OK" : " FAIL"));
                int hCommit = ps.Commit();
                log.Add("Commit()             hr=0x" + hCommit.ToString("X8") + (hCommit == 0 ? " OK" : " FAIL"));
                PropVariantClear(ref pv);
                log.Add("readback(API)        = " + GetFriendlyName(deviceId));
            }
            return string.Join("\n", log.ToArray());
        }

        public static string HResultName(int hr)
        {
            switch (unchecked((uint)hr))
            {
                case 0x80070005: return "E_ACCESSDENIED";
                case 0x8007000E: return "E_OUTOFMEMORY";
                case 0x80004005: return "E_FAIL";
                case 0x80070057: return "E_INVALIDARG";
                case 0x88890001: return "AUDCLNT_E_NOT_INITIALIZED";
                case 0x80070490: return "ERROR_NOT_FOUND";
                default: return "0x" + unchecked((uint)hr).ToString("X8");
            }
        }
    }
}
