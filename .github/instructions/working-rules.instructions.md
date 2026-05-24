# working-rules instructions

> This file defines the operational rules for the AI. It specifies how the AI must behave.

## **Strict Rules**

 - Before beginning implementation, formulate a plan. If you do not have 100% confidence in that plan, revise and rebuild it as many times as necessary until you are fully confident.
 - When implementing, you must always add corresponding test code as well.
 - After completing the implementation, verify that there are no defects, vulnerabilities, or regressions. Repeat this verification as many times as necessary until you are 100% confident that none exist.
  - Should it become apparent that the implementation contains any defects, vulnerabilities, or regressions, fix them immediately. Even after the fixes, repeat the verification as many times as necessary until you are 100% confident that none remain.
 - Once implementation and corrections have been completed, launch the review /Subagent and have it conduct a rigorous review. All points raised in the review must be addressed. After making the corrections, repeat the verification as many times as necessary until you are 100% confident that no issues remain.
 - These rules must be strictly observed in order to maintain the quality of the project. Failure to comply with these rules constitutes a failure to fulfill your responsibilities toward the project.
 - ソースコードを作成・編集したときには **必ず UI テストを実行** し、指示された機能が正しく実装・修正されていることを確認すること。