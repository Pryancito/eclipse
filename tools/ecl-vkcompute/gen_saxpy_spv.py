#!/usr/bin/env python3
"""Emit a Vulkan 1.0 compute SAXPY SPIR-V (BufferBlock SSBOs + push constant).

    z[i] = a * x[i] + y[i]   local_size = 32

Hand-written so the ISO does not need glslang. SPIR-V 1.0 + BufferBlock +
Uniform (not StorageBuffer) matches `VK_API_VERSION_1_0` in ecl-vkcompute.c.
"""
from __future__ import annotations

import struct
from pathlib import Path

# SPIR-V opcodes / enums we need (unified1, little-endian words).
OpCapability = 17
OpExtInstImport = 11
OpMemoryModel = 14
OpEntryPoint = 15
OpExecutionMode = 16
OpDecorate = 71
OpMemberDecorate = 72
OpTypeVoid = 19
OpTypeFunction = 33
OpTypeFloat = 22
OpTypeInt = 21
OpTypeVector = 23
OpTypePointer = 32
# 28 is OpTypeArray (needs a length id). Runtime arrays are 29.
# Using 28 here made spirv_to_nir return NULL and NVIDIA's compiler SIGSEGV.
OpTypeRuntimeArray = 29
OpTypeStruct = 30
OpConstant = 43
OpVariable = 59
OpFunction = 54
OpFunctionEnd = 56
OpLabel = 248
OpLoad = 61
OpStore = 62
OpAccessChain = 65
OpCompositeExtract = 81
OpFMul = 133
OpFAdd = 129
OpReturn = 253

CapabilityShader = 1
AddressingLogical = 0
MemoryGLSL450 = 1
ExecGLCompute = 5
LocalSize = 17
DecorationBufferBlock = 3
DecorationBlock = 2
DecorationArrayStride = 6
DecorationBuiltIn = 11
DecorationNonWritable = 24
DecorationBinding = 33
DecorationDescriptorSet = 34
DecorationOffset = 35
BuiltInGlobalInvocationId = 28
StorageUniform = 2
StorageInput = 1
StoragePushConstant = 9
StorageBuffer = 12
FunctionControlNone = 0


def _str_words(s: str) -> list[int]:
    raw = s.encode("utf-8") + b"\x00"
    while len(raw) % 4:
        raw += b"\x00"
    out = []
    for i in range(0, len(raw), 4):
        out.append(struct.unpack_from("<I", raw, i)[0])
    return out


def _insn(opcode: int, *operands: int) -> list[int]:
    words = [0, *operands]
    words[0] = (len(words) << 16) | opcode
    return words


def build() -> bytes:
    # IDs — separate types per SSBO, like glslang. Sharing one struct for three
    # bindings made lavapipe return VK_ERROR_UNKNOWN at pipeline compile.
    t_void, t_float, t_uint, t_v3u = 1, 2, 3, 4
    t_ptr_in_v3u, v_gid, glsl = 5, 6, 7
    rt_x, st_x, ptr_x, v_x = 8, 9, 10, 11
    rt_y, st_y, ptr_y, v_y = 12, 13, 14, 15
    rt_z, st_z, ptr_z, v_z = 16, 17, 18, 19
    t_pc, t_ptr_pc, v_pc = 20, 21, 22
    t_ptr_f_x, t_ptr_f_y, t_ptr_f_z, t_ptr_pc_f = 23, 24, 25, 26
    c_0, t_fn, f_main, l_start = 27, 28, 29, 30
    gid_ld, idx = 31, 32
    p_x, p_y, p_z = 33, 34, 35
    x_v, y_v, p_a, a_v, ax, res = 36, 37, 38, 39, 40, 41
    bound = 42

    body: list[int] = []
    body += _insn(OpCapability, CapabilityShader)
    body += _insn(OpExtInstImport, glsl, *_str_words("GLSL.std.450"))
    body += _insn(OpMemoryModel, AddressingLogical, MemoryGLSL450)
    # SPIR-V 1.0 interface is Input/Output only — do not list SSBOs / PC.
    body += _insn(OpEntryPoint, ExecGLCompute, f_main, *_str_words("main"), v_gid)
    body += _insn(OpExecutionMode, f_main, LocalSize, 32, 1, 1)

    body += _insn(OpDecorate, v_gid, DecorationBuiltIn, BuiltInGlobalInvocationId)
    for rt, st, var, binding, ro in (
        (rt_x, st_x, v_x, 0, True),
        (rt_y, st_y, v_y, 1, True),
        (rt_z, st_z, v_z, 2, False),
    ):
        body += _insn(OpDecorate, rt, DecorationArrayStride, 4)
        body += _insn(OpDecorate, st, DecorationBufferBlock)
        body += _insn(OpMemberDecorate, st, 0, DecorationOffset, 0)
        body += _insn(OpDecorate, var, DecorationDescriptorSet, 0)
        body += _insn(OpDecorate, var, DecorationBinding, binding)
        if ro:
            body += _insn(OpDecorate, var, DecorationNonWritable)
    body += _insn(OpDecorate, t_pc, DecorationBlock)
    body += _insn(OpMemberDecorate, t_pc, 0, DecorationOffset, 0)

    body += _insn(OpTypeVoid, t_void)
    body += _insn(OpTypeFloat, t_float, 32)
    body += _insn(OpTypeInt, t_uint, 32, 0)
    body += _insn(OpTypeVector, t_v3u, t_uint, 3)
    body += _insn(OpTypePointer, t_ptr_in_v3u, StorageInput, t_v3u)
    body += _insn(OpVariable, t_ptr_in_v3u, v_gid, StorageInput)
    for rt, st, ptr, var in (
        (rt_x, st_x, ptr_x, v_x),
        (rt_y, st_y, ptr_y, v_y),
        (rt_z, st_z, ptr_z, v_z),
    ):
        body += _insn(OpTypeRuntimeArray, rt, t_float)
        body += _insn(OpTypeStruct, st, rt)
        body += _insn(OpTypePointer, ptr, StorageUniform, st)
        body += _insn(OpVariable, ptr, var, StorageUniform)
    body += _insn(OpTypeStruct, t_pc, t_float)
    body += _insn(OpTypePointer, t_ptr_pc, StoragePushConstant, t_pc)
    body += _insn(OpVariable, t_ptr_pc, v_pc, StoragePushConstant)
    body += _insn(OpTypePointer, t_ptr_f_x, StorageUniform, t_float)
    body += _insn(OpTypePointer, t_ptr_f_y, StorageUniform, t_float)
    body += _insn(OpTypePointer, t_ptr_f_z, StorageUniform, t_float)
    body += _insn(OpTypePointer, t_ptr_pc_f, StoragePushConstant, t_float)
    body += _insn(OpConstant, t_uint, c_0, 0)
    body += _insn(OpTypeFunction, t_fn, t_void)

    body += _insn(OpFunction, t_void, f_main, FunctionControlNone, t_fn)
    body += _insn(OpLabel, l_start)
    body += _insn(OpLoad, t_v3u, gid_ld, v_gid)
    body += _insn(OpCompositeExtract, t_uint, idx, gid_ld, 0)
    body += _insn(OpAccessChain, t_ptr_f_x, p_x, v_x, c_0, idx)
    body += _insn(OpAccessChain, t_ptr_f_y, p_y, v_y, c_0, idx)
    body += _insn(OpAccessChain, t_ptr_f_z, p_z, v_z, c_0, idx)
    body += _insn(OpLoad, t_float, x_v, p_x)
    body += _insn(OpLoad, t_float, y_v, p_y)
    body += _insn(OpAccessChain, t_ptr_pc_f, p_a, v_pc, c_0)
    body += _insn(OpLoad, t_float, a_v, p_a)
    body += _insn(OpFMul, t_float, ax, a_v, x_v)
    body += _insn(OpFAdd, t_float, res, ax, y_v)
    body += _insn(OpStore, p_z, res)
    body += _insn(OpReturn)
    body += _insn(OpFunctionEnd)

    header = [0x07230203, 0x00010000, 0, bound, 0]
    return struct.pack(f"<{len(header) + len(body)}I", *header, *body)


def main() -> None:
    here = Path(__file__).resolve().parent
    spv = build()
    (here / "saxpy.spv").write_bytes(spv)
    lines = [
        "/* Generated by gen_saxpy_spv.py — do not edit. */",
        f"static const unsigned char k_saxpy_spv[{len(spv)}] __attribute__((aligned(4))) = {{",
    ]
    chunk = []
    for i, b in enumerate(spv):
        chunk.append(f"0x{b:02x}")
        if len(chunk) == 12:
            lines.append("    " + ", ".join(chunk) + ",")
            chunk = []
    if chunk:
        lines.append("    " + ", ".join(chunk) + ",")
    lines.append("};")
    (here / "saxpy_spv.h").write_text("\n".join(lines) + "\n")
    print(f"wrote saxpy.spv ({len(spv)} B) and saxpy_spv.h")


if __name__ == "__main__":
    main()
