/* -*- Mode: IDL; tab-width: 2; indent-tabs-mode: nil; c-basic-offset: 2 -*- */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/media-source/#sourcebufferlist

[Exposed=Window]
interface SourceBufferList : EventTarget {
  readonly attribute unsigned long length;
  attribute EventHandler onaddsourcebuffer;
  attribute EventHandler onremovesourcebuffer;
  getter SourceBuffer (unsigned long index);
};
