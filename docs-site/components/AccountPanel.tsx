"use client";

import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/silicon-ui/button/button";
import { createAccountController } from "@/lib/account.mjs";

export default function AccountPanel() {
  const [account, setAccount] = useState({
    status: "Sign in as a Carbon or Silicon",
    signedIn: false,
    loginBusy: false,
    logoutBusy: false,
  });
  const controller = useRef<ReturnType<typeof createAccountController> | null>(null);

  useEffect(() => {
    const current = createAccountController(setAccount);
    controller.current = current;
    return () => {
      current.dispose();
      controller.current = null;
    };
  }, []);

  return <section className="account-panel" aria-label="Silicon Accounts">
    <span id="account-status" role="status">{account.status}</span>
    {account.signedIn
      ? <Button id="account-logout" type="button" variant="secondary" size="sm" loading={account.logoutBusy} onClick={() => void controller.current?.signOut()}>Sign out</Button>
      : <Button id="account-login" type="button" variant="secondary" size="sm" loading={account.loginBusy} onClick={() => void controller.current?.signIn()}>Sign in</Button>}
  </section>;
}
